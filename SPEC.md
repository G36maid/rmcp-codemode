# rmcp-codemode v2 — Design Specification

> **Status:** DRAFT v0.1. Decisions marked **LOCKED** were settled in design review
> (November 2026) and carry source evidence. Items marked **OPEN** are pending.
>
> **Supersedes:** the v1 architecture (git history through `ded98f6`). v1 remains in
> the tree as a running reference implementation and a 55-test behavior corpus; its
> disposition is OQ1.
>
> **Landscape verified against (2026-11):** pi (`badlogic/pi-mono`, source audited),
> OpenCode codemode (source audited), `openai/codex` `codex-rs` Code Mode (source +
> DeepWiki audited), Claude Code Code Mode v2.1.323 (official docs + changelog).

## 1. Vision

**A Rust library crate that consumes N upstream MCP servers and exposes them to any
MCP host as a single `codemode` tool.** The model writes JavaScript that calls every
upstream tool — parallel fan-out, chaining, filtering — and only the distilled result
returns to the model.

- **Crate, not binary** (v2). Embedders (Zed first) own transports, configuration,
  and process lifecycle; they may also register their own native tools.
- **Host-agnostic.** Any MCP client — Zed, Claude Code, Codex, pi, anything — gets
  code mode by wiring one handler. No host-side extension required.
- The niche is validated by the landscape: pi/OpenCode are TypeScript host
  extensions, Codex's code mode is an embedded V8 subsystem of codex-rs, Claude
  Code's is a first-party REPL over its own tools. **No one offers host-agnostic
  MCP aggregation as a reusable Rust library.**

## 2. Landscape (why the design looks like this)

| | pi | OpenCode | Codex (`codex-rs`) | Claude Code |
|---|---|---|---|---|
| Language / engine | TS + QuickJS/run | TS + QuickJS | **Rust + V8 isolate** | n/a (first-party) |
| Model-facing tools | one `codemode` | one `codemode` | `exec` + `wait` (freeform grammar) | one `REPL` |
| Lifecycle | fresh VM; approval suspends VM, host replays | fresh VM | **cells**: yield, script stays live, `wait(cell_id)` observes | classifier reviews whole script |
| Session state | store (committed at run end) | same | `store`/`load` persist across execs (session-scoped) | — |
| Naming | `mcp__<server>__<tool>` flat | same family | `mcp__<server>__<tool>` flat | host nouns (`$`, `edit()`) |
| Deferred tools | yes (never listed; stable description) | yes (counted search, stub listing) | yes (`ALL_TOOLS` + ranked search) | — |
| Determinism | virtual clock + mulberry32 | none | none | none |

Key reads that shaped decisions:

- **Codex is Rust** — proves the language choice; but it is embedded (V8 bound to
  their tool runtime), not reusable. Not a competitor for the library niche.
- **Codex issue #37825**: freeform-grammar tools break on function-only providers;
  JSON function tools (exactly MCP's tool shape) are the compatible side. Our
  single JSON tool with a `script` parameter is on the right side of this.
- **Convergent core across all four**: model writes JS; nested tools are async
  functions on a global object; `Promise.all` = parallelism; output is content
  items (`text`/`image`) with only distilled results returned; `text()`/`image()`/
  `exit()` helpers; `store()`/`load()` session persistence; budget trio
  (output tokens, deadline, call accounting); **nested calls always re-enter the
  host's permission machinery** (pi approvals, codex hooks, CC hooks/classifier,
  OC explicit failure).
- **The one true lifecycle schism**: pi suspends the VM and replays (deterministic,
  host-controlled) vs Codex keeps the script live and streams observations
  (model/script time-sharing). These are different architectures, not two spellings
  of one idea. v2 picks pi (D3) and does not attempt to abstract over both.

## 3. Decision log

| # | Decision | Choice | Status | Evidence / rationale |
|---|---|---|---|---|
| D1 | Positioning | single-MCP-router **library**: N upstreams → one `codemode` tool; Zed is the anchor consumer | LOCKED | §1; unoccupied niche (§2) |
| D2 | Form | **crate only**, no binary; transports are the embedder's concern | LOCKED | design review 2026-11 |
| D3 | Execution lifecycle | **pi model**: fresh QuickJS VM per run; suspension/replay as engine capability | LOCKED | pi `runtime/worker.ts:2` *"one worker runs one script inside a fresh QuickJS VM"*; `execution.ts:21` (interrupt + dispose) |
| D4 | Tool naming | **`mcp__<server>__<tool>`**, flat | LOCKED | pi `core/mcp-servers.ts:47-49`; codex exec description (`tools.mcp__ologs__get_profile`) |
| D5 | Approval over MCP wire | **not in v2** — runs execute directly within budgets; engine keeps `SuspendingEngine` for embedders and a future wire design (elicitation is the likely vehicle) | LOCKED | design review 2026-11 |
| D6 | Native tools | **embedder's right**: the crate accepts native tools into the Space; the crate itself ships none | LOCKED | pi `tool.ts:164-166` (`getCodemodeCallableTools` = all session tools minus codemode itself); CC `$`/`edit()` |
| D7 | Script surface | **convergent intersection is core**; per-implementation extras are opt-in profile items (§6.2) | LOCKED (core) | §2 convergent core list |
| D8 | Conformance testing | **pi's own test suite, ported mechanically, is the conformance instrument** (same scripts, fixtures, assertions; each test labeled with its pi source); real-MCP fixtures on the ingestion axis; mocks only for engine-internal units | LOCKED | v1's hand-written tests verify the porter's *impression* of pi, not pi — rejected in review; the ported suite is the oracle |
| D9 | Tool description | catalog lives in the **codemode tool description** (not system prompt): TS declarations grouped by namespace, budgeted, deferred-aware | LOCKED | pi `tool.ts:229-272`; §7 |
| D10 | Budget constants | pi-aligned (§5.2) | LOCKED | pi `execute.ts:52,306-307`; v1 `Budget::default()` matches |
| D11 | Determinism | virtual clock (advances only on delivered outcomes, never backward) + seeded mulberry32; WeakRef/FinalizationRegistry removed | LOCKED | pi `vm.ts:83-103`; `protocol.ts:24` |

## 4. Architecture — crate layering

Proposed split (**OQ2 — resolved: keep v1's two-crate split**; core contracts
without a JS engine, quickjs carrying engine + server handler. The engine is
*not* required to be rmcp-free — the router library is consumed as a whole):

```
rmcp-codemode            core contracts, no JS:
                         Space (tool catalog), Catalog (description rendering),
                         Budget, Engine trait, SuspendingEngine trait,
                         SuspendState / Determinism / results,
                         upstream ingestion (space_from_peer), naming rules
        ▲
        │
rmcp-codemode-quickjs    QuickJS engine + rmcp ServerHandler component:
fresh VM per run,        one codemode tool (+ optional direct tools per Mode),
bindings, proxy tree,    catalog description, store domains via _meta,
dispatch, budgets,       suspension/pause API surface for embedders
determinism,
suspension/replay        transports wired by the EMBEDDER
```

Layering rules:

- `quickjs` depends on core. The core crate never links a JS engine; the rmcp
  dependency spans both crates by design (the integration surface is the
  whole router library).
- An embedder drives the engine trait directly for headless testing; MCP
  hosts consume the `ServerHandler`.

## 5. Engine contract (pi model)

### 5.1 Execution lifecycle

- **Fresh VM per run.** A run gets a new QuickJS VM; the VM is disposed when the
  run settles, times out, or is aborted (D3).
- Preemption via interrupt handler at the deadline; a never-settling tool promise
  fails the run immediately; unawaited calls are cancelled.
- **Worker pool** (run-level): default 2 slots, arrival order, admission failure
  *before* the VM starts, wait counted against the deadline, pi's exact admission
  failure message. Worker-pool wait and deadline interact as `min(deadline, wait)`.
- **In-flight call limiter** (call-level): pi caps concurrent in-flight nested
  calls separately from run slots (`execute.ts:265-270`). v1 lacks this layer —
  **OQ9**.

### 5.2 Budgets (defaults LOCKED, D10)

| Budget | Default | Notes |
|---|---|---|
| `timeout_ms` | host decides (None = none) | script `@options.timeout_ms` may tighten |
| `max_output_tokens` | 10,000 (4 chars/token estimate) | pi `execute.ts:306-307` |
| `max_output_items` | 100,000 | |
| `max_output_chars` | 16 MiB | |
| `max_tool_calls` | 500 | counted calls include `searchTools` (OC semantics) |
| `default_search_limit` | 8 (pi's; OC's is 10) | script's explicit `limit` wins |
| `memory_limit` | 256 MiB | pi's heap cap |
| `max_workers` / `worker_wait_ms` | 2 / 60,000 ms | pi's process-wide worker story |

### 5.3 Suspension & replay (capability, not wire)

- `SuspendingEngine`: host-controlled runs record calls; the host answers via
  `deliver(seq, now, id, outcome)`; resume replays recorded outcomes (pi
  `execution.ts:169,260-283,367`). Serializable `SuspendState`; resumed runs stay
  host-controllable (may pause again).
- **Not exposed over MCP in v2** (D5). The server component may still expose the
  embedder-level pause/resume API.

### 5.4 Determinism (D11)

- `Determinism { started_at_ms, seed }` on the run context; virtual clock starts
  at `started_at_ms`, advances **only** on delivered outcomes, never backward;
  `Date` reads the clock; `Math.random` = mulberry32(seed); no WeakRef /
  FinalizationRegistry; deterministic config travels in `SuspendState`.

### 5.5 VM fingerprint (NEW — pi parity gap)

Every run reports a **fingerprint** before its first call (pi `types.ts:217-218`
`onStarted?(fingerprint)`, `vm.ts:318`): engine version, bindings/prelude hash,
determinism-semantics version, stack/heap/budget, interrupt-poll count. Resume
refuses a `SuspendState` whose recorded fingerprint differs (replay safety).
Composition details: **OQ5**.

## 6. Script-facing surface

### 6.1 Core (intersection of all four — LOCKED, D7)

- Global `tools` object; nested tools are async functions; `Promise.all` /
  `Promise.allSettled` = parallelism.
- `text(value)`, `image(...)`, `exit(...)` output helpers; final `return` value
  becomes output.
- `store(key, value)` / `load(key)` — reads see the run's initial snapshot,
  writes buffer, commit only on success (pi split; v1 semantics kept).
  Store domains via `_meta["codemode/storeDomain"]` (SEP-1319) kept.
- `ALL_TOOLS` metadata (`{ name, description }`).
- Budgets per §5.2 enforced inside the sandbox; over-budget emits the pi-style
  error strings.

### 6.2 Per-item status

| Item | Status | Notes |
|---|---|---|
| `console` (total formatter: cycles → `[Circular]`, NaN/Infinity literal, never throws) | **KEEP** | pi/OC have console; codex removes it — rejected here. OC's total-formatting semantics adopted (contract-tested) |
| `searchTools` / `describeTool` / `describeNamespace` | **KEEP** | counted against `maxToolCalls`, observed in call records (OC semantics); BM25 non-negative IDF. Layer placement: **OQ6** (pi has these at the host-extension layer, not engine core) |
| `notify()` (mid-run output injection) | **OQ8** | codex-only; MCP has no mid-call partial results today |
| `setTimeout` / `clearTimeout` | **OQ8** | codex-only; pending timeouts must not keep the run alive |
| `yield_control()` / cell primitives | **OUT** | tied to the cell lifecycle (§2 schism); rejected for v2 |
| `generatedImage(...)` | **OQ8** | codex-only; possibly covered by `image()` |

## 7. Tool catalog & description

The codemode tool's description carries the catalog (D9):

- Structure: intro + globals documentation + `Nested tools:` — one section per
  listed tool (TypeScript declaration + guidelines), grouped by namespace;
  ungrouped first, then namespaces by name (pi `tool.ts:258-272`).
- **Inline budget**: 3,000 tokens × 4 chars/token, pi's round-robin packing —
  each round every group places its cheapest remaining section; a group that
  cannot fit drops out while others continue; every namespace is represented
  before any is complete. **v1 already implements this** (`catalog.rs:254-280`).
- Exposure-aware listing: `Mode::On` lists non-direct tools; `Mode::Only` lists
  all (v1 `catalog.rs:148-159`; pi `tool.ts:360-366`).

### 7.1 Deferred tools (REALIGN — was mis-stated in v1)

pi's deferred semantics: deferred tools are **never listed** and do not affect the
description at all, so the description stays byte-stable while tools register late
(*"for example by MCP servers that connect"* — `tool.ts:229-231`); discovery is via
`ALL_TOOLS` / `searchTools`. Codex carries the same concept
(`DEFERRED_NESTED_TOOLS_GUIDANCE`). v1's `Exposure::Deferred` **is listed** in
`Mode::On` — a mismatch to fix.

v2 must define the listing policy per `Exposure × Mode` and the behavior under
dynamic upstreams (**OQ3**):

- Does a late-connecting upstream's tools start deferred and get promoted?
- On upstream disconnect, are its listed sections withdrawn (description changes)
  or marked unavailable?
- MCP has `tools/list_changed`: pi chose byte-stability for prompt-cache
  friendliness without a change-notification channel; an MCP router *has* one.
  Byte-stability vs redeclare-on-change is the core of **OQ3**.

## 8. Naming & upstream ingestion

- Canonical tool name: `mcp__<server>__<tool>` (D4), one flat segment in the
  Space, so `tools.mcp__github__list_issues` is a plain property read.
  Prefix-level access (`tools["mcp__github__" + name]`) must resolve as the
  namespace (pi `tool.ts:413-414`).
- Normalization rules for server/tool names that are not valid JS identifiers
  (pi and codex both normalize; exact tables differ) — **OQ4**, plus the
  collision policy for normalized names.
- Ingestion reuses v1's `space_from_peer` / `upstream_into_space`: handshake
  `instructions` auto-attached, unwrap order (structuredContent → first text
  block JSON → raw text), `isError` → `Err` carrying text.
- Upstream lifecycle API (library level): connect/reconnect with backoff, health,
  tool refresh, per-server enable/disable — **OQ7**.

## 9. Native tools (embedder's right)

- `NativeTool` (v1 `server.rs:115`) stays: the embedder registers native tools
  (read/write/bash/…) into the Space alongside `mcp__*` tools; per-tool
  `Exposure` (Direct / Deferred / CodeModeOnly) controls `tools/list` and
  description listing (D6).
- The crate ships **no** built-in native tools. If an embedder wants a bash
  tool, it is the embedder's bash — one source of truth for permissions and
  sandboxing.

## 10. Server handler component

- rmcp `ServerHandler` exposing: one `codemode` tool (+ optional direct tools per
  `Mode::On` / `Mode::Only`), the catalog description (§7), store-domain scoping
  via `_meta` (handler reads `RequestContext.meta` — rmcp 3.9 quirk), and the
  embedder-level pause/resume API (§5.3).
- Transports are the embedder's (D2). The component is transport-agnostic; the
  demo's in-process wiring remains the reference.

## 11. Conformance & testing

### 11.1 The oracle: pi's own suite, ported mechanically

v1's hand-written tests encode the porter's impression of pi and are **not** the
standard. The standard is pi's codemode test suite
(`packages/codemode/test/`, 117 cases), ported one-by-one: **same JS script
strings, same fixtures, same assertions**, each Rust test labeled with its pi
source (`pi:sandbox.test.ts > tools > aborts unawaited calls…`). A ported test
that cannot apply to the Rust engine (TS-worker specifics) is documented `N/A`
with the reason — never silently dropped. Every failing port is a semantic
divergence to fix in the engine, not a test to weaken.

Inventory:

| pi file | cases | covers (already-visible v1 divergences) |
|---|---|---|
| `sandbox.test.ts` | 55 | script execution, output order, exit()/partial-output-on-failure, **V8-style stacks without prelude frames**, line-numbered errors, non-Error throws, tools (records, concurrency, normalized identifiers, unawaited-call aborts, close-match naming), store (snapshot+writes, copies, in-script oversized rejection), globals (namespaced, **argument spreading**, reserved-name rejection), limits (**uncatchable abort error**, never-settling promise, output limits hold even when caught, parallel runs share nothing), escape hatches (no host globals, eval/Function caged, dynamic import rejected, built-in patches ignored) |
| `deterministic.test.ts` | 24 | same calls/output for same start/seed/results; UTC regardless of host tz; **results applied in seq order regardless of delivery order**; **fingerprint before first call**; **clock monotonic + rejects non-finite delivered `now_ms`**; unserializable answers leave the call waiting; abort = uncatchable; double-delivery rejected |
| `declarations.test.ts` | 19 | `schemaToType`: budget→unknown, draft-04 constraints, `$ref` resolution with recursion stop, arrays/tuples — the description catalog's declaration renderer |
| `portable.test.ts` | 7 | port with adaptation notes |
| `worker-lifetime.test.ts` | 7 | TS-worker specifics — likely `N/A` (Rust threading differs); document per-case |
| `source.test.ts` | 5 | embedded sources parse; port |

Known v1 gaps already legible from test names (to be confirmed by the port):
V8-style stack formatting, uncatchable abort errors, non-finite clock-value
rejection, seq-order application, namespaced globals with argument spreading,
registration-time reserved-name rejection, and the full `schemaToType`
renderer (v1's `ts_declaration` is a simplification).

### 11.2 Real-MCP ingestion fixtures (D8)

rmcp in-process servers over real wire semantics, plus one spawned stdio
subprocess upstream in CI; naming conformance (`mcp__<server>__<tool>`,
normalization tables) tested against live upstream tool names.

### 11.3 OpenCode-adopted semantics

The OC-adopted behaviors (counted `searchTools`, total console formatting,
tool-path probes: `prototype`/`__proto__`/poisoning/bracket names, close-match
guard) get the same treatment from OC's four test files
(`codemode/errors/limits/tool-paths.test.ts`): mechanical ports, labeled.

### 11.4 Status of v1's tests

v1's 55 tests are demoted: where they agree with a ported oracle test they are
redundant; where they conflict, **the ported pi suite wins**; where they encode
non-pi behavior we deliberately adopted (e.g. OpenCode semantics above), they
are superseded by the OC ports of §11.3.

## 12. Out of scope (v2)

- Wire-level approval / elicitation (engine capability remains; D5).
- Binary packaging, config files, stdio/HTTP server wiring (D2).
- Cell/wait persistent-cell lifecycle (codex model) — rejected for v2 (§2).
- A V8 engine implementation — the Engine trait permits one; not planned.

## 13. Open questions

| # | Question | Options on the table |
|---|---|---|
| OQ1 | ~~v1 disposition~~ **RESOLVED** | salvage the engine code, but **replace the test authority**: port pi's 117-case suite mechanically (§11.1); failing ports are engine work items. v1's hand-written tests demoted (§11.4). Crate structure stays two crates (OQ2 resolved: keep v1's split; §4 revised) |
| OQ2 | ~~Crate split~~ **RESOLVED** | keep v1's two crates (core contracts / quickjs engine + server handler); revisit only if the server layer grows worth independent release |
| OQ3 | Deferred listing policy + description stability | deferred-by-default for upstreams; promotion policy; byte-stability vs `tools/list_changed` redeclare |
| OQ4 | Name normalization | charset rules for server/tool names → JS identifiers; normalized-name collision policy |
| OQ5 | Fingerprint composition | which hashes/versions enter the string; where it lives in `SuspendState` |
| OQ6 | searchTools layer placement | bindings layer (v1) vs host/extension layer (pi's split) |
| OQ7 | Upstream lifecycle API | reconnect/backoff shape, refresh triggers, enable/disable |
| OQ8 | codex-only globals | `notify()`, `setTimeout`, `generatedImage` adopt or reject |
| OQ9 | In-flight call limiter | adopt pi's second limiter layer; default limit |
