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
| D5 | Approval over MCP wire | **not in v2** — runs execute directly within budgets; engine keeps `SuspendingEngine` for embedders and a future wire design. **The standard vehicle is the MCP Tasks extension** (SEP-1686 → SEP-2663 Final): a run needing approval = task in `input_required` with the elicitation in `inputRequests`, answered via `inputResponses`; long-running scripts = task-augmented `tools/call` with `tasks/get` polling (rmcp 3.9 ships `task_manager.rs`). v1's bespoke `{status:"paused", executionId}` wire shape is superseded by this for any future wire work | LOCKED | spec 2025-11-25 `basic/utilities/tasks`; SEP-2663 (Final 2026-04) |
| D6 | Native tools | **embedder's right**: the crate accepts native tools into the Space; the crate itself ships none | LOCKED | pi `tool.ts:164-166` (`getCodemodeCallableTools` = all session tools minus codemode itself); CC `$`/`edit()` |
| D7 | Script surface | **convergent intersection is core**; per-implementation extras are opt-in profile items (§6.2) | LOCKED (core) | §2 convergent core list |
| D8 | Conformance testing | **pi's own test suite, ported mechanically, is the conformance instrument** (same scripts, fixtures, assertions; each test labeled with its pi source); real-MCP fixtures on the ingestion axis; mocks only for engine-internal units | LOCKED | v1's hand-written tests verify the porter's *impression* of pi, not pi — rejected in review; the ported suite is the oracle |
| D9 | Tool description | catalog lives in the **codemode tool description** (not system prompt): TS declarations grouped by namespace, budgeted, deferred-aware | LOCKED | pi `tool.ts:229-272`; §7 |
| D10 | Budget constants | pi-aligned (§5.2) | LOCKED | pi `execute.ts:52,306-307`; v1 `Budget::default()` matches |
| D11 | Determinism | virtual clock (advances only on delivered outcomes, never backward) + seeded mulberry32; WeakRef/FinalizationRegistry removed | LOCKED | pi `vm.ts:83-103`; `protocol.ts:24` |
| D12 | MCP protocol baseline | spec **2025-11-25** + the Final SEPs we rely on (§10.1); `_meta` keys use reverse-DNS prefix `io.github.g36maid/` (v1's `codemode/storeDomain` violates the SHOULD and is renamed); tool naming complies with SEP-986; Tasks extension tracked as the future wire vehicle (D5) | LOCKED | verified against modelcontextprotocol.io 2026-11 |
| D13 | Upstream aggregation semantics | **pi-exact** (§8.2): tools + resources-as-tools (no prompts); `isError` resolves to scripts (model-facing output marked), v1's Err-rejection dropped; annotation hints forwarded, icons/`_meta` stripped; minimal client capabilities; truncated output saved with `fullOutputPath`; auth = embedder's boundary (crate consumes connected peers) | LOCKED | pi `tools.ts:8-10,135-232,275-278`; `resources.ts:53-55`; `index.ts:387`; verified 2026-11 |
| D14 | Engine contract shape | (a) no Space — flat tool lists, catalog/search at server layer; (b) `execute` + `start`/`deliver` only, replay is host-layer, v1's internal replay dispatcher dropped; (c) abort = `CancellationToken`; (d) types mirror `types.ts` 1:1 (§5.0) | LOCKED | pi `types.ts` full read 2026-11; user-approved |

## 4. Architecture — crate layering

Proposed split (**OQ2 — resolved: two-crate structure**, carried into the
rewrite; core contracts
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

### 5.0 Contract shape (D14 — locked 2026-11)

Four locked decisions from the `types.ts` full read:

- **(a) No Space in the engine contract.** The engine takes pi's flat lists —
  `tools: Vec<Tool>` and `globals: Vec<Tool>` — plus per-run declared tools
  for host-controlled runs. v1's hierarchical `Space` is gone from the core
  crate; catalog rendering (§7), the search index, and `searchTools` wiring
  live at the server layer (§6.1, OQ3/OQ6 consequences).
- **(b) `execute` + `start`/`deliver` are the only mechanisms.** Mode 1
  (`execute(script, options)`): run-to-completion, host-supplied tool
  closures, returns the full result. Mode 2 (`start(runOptions)`):
  host-controlled — the engine receives only **declarations**
  (`DeclaredTool {name, js_name, description}`), reports
  `on_call(HostCall {id, target, name, args})`, and the host answers via
  `run.deliver(seq, now, id, outcome)`: results numbered 1,2,… without gaps,
  applied strictly in seq order regardless of arrival order; the clock reads
  `now` while processing, never backward; `on_started(fingerprint)` precedes
  any call; `during {seq, issued}` reports what was in flight when the script
  ended. **Replay is host-layer**: a resumed run is a fresh `start()` fed the
  recorded (calls, outcomes, started_at, seed, fingerprint) — the
  deterministic guarantees make it converge. v1's engine-internal
  `Dispatcher::Replay` is not carried over; `SuspendState` becomes a
  host-side record type.
- **(c) Abort signal** = `tokio_util::sync::CancellationToken` (pi
  `AbortSignal`'s Rust equivalent; aborted on finish/timeout/abort/close).
- **(d) Core types mirror `types.ts` 1:1** — `Tool`, `OutputItem`, `Call`,
  `RunResult`, `HostCall`, `Outcome`, `During`, `RunOptions`, `Run`,
  `ExecuteOptions`, `SandboxOptions` — snake_case fields, each doc-commented
  with its pi source name for port traceability.

### 5.1 Execution lifecycle

- **Fresh VM per run.** A run gets a new QuickJS VM; the VM is disposed when the
  run settles, times out, or is aborted (D3).
- Preemption via interrupt handler at the deadline; a never-settling tool promise
  fails the run immediately; unawaited calls are cancelled.
- **Worker pool** (run-level): default 2 slots, arrival order, admission failure
  *before* the VM starts, wait counted against the deadline, pi's exact admission
  failure message. Worker-pool wait and deadline interact as `min(deadline, wait)`.
- **No in-flight call limiter** (RESOLVED, OQ9 — pi-exact): nested calls are
  unlimited within a run, bounded only by `max_tool_calls` and the deadline —
  pi's engine has no concurrency cap on nested calls either (the prelude
  bridge settles each call as it lands); pi's only `createLimiter`
  (`execute.ts:50,265-270,750`, `MAX_CONCURRENT_MODEL_CALLS = 4`) guards
  `models`-namespace sub-model calls, a host-specific surface ours does not
  have. Fan-out pressure is the **upstreams'** problem (MCP servers own their
  concurrency) and the embedder's (transport-level connection pools). An
  optional `Budget.max_in_flight_calls` remains an **additive** future knob:
  it is script-invisible (all calls still complete, records stay in issue
  order, virtual clock unaffected), so adding it later breaks nothing.

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
| `max_stack_size` | 512 KiB | pi's default (`vm.ts:55-58`); enters the fingerprint (§5.5) |
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

### 5.5 VM fingerprint (RESOLVED, OQ5 — pi-exact)

Every deterministic run reports a **fingerprint** before its first call —
the `started` message precedes any call (pi `types.ts:217-218`,
`deterministic.test.ts:180` asserts the `started → call → idle` order).
Composition is pi's exact slot list (`vm.ts:319`), mapped to our stack:

```
rquickjs {version}, quickjs {libqjs version}, prelude {fnv1a-8hex(prelude source)},
deterministic {DETERMINISTIC_VERSION}, stack {Budget.max_stack_size},
heap {Budget.memory_limit}, budget none
```

- **prelude hash**: FNV-1a 32-bit (`0x811c9dc5` offset, `0x01000193` prime),
  8 hex chars, over the full prelude JS source — pi `vm.ts:72-81`. Any
  prelude change changes the fingerprint.
- **`DETERMINISTIC_VERSION`** (`= 1`, core crate constant): bumped whenever a
  replay-visible semantic changes (PRNG algorithm, UTC date behavior, result
  application order). Version discipline guarded by pinned-value tests, as in
  pi (`deterministic.test.ts:132`: *"a change here changes what replayed
  scripts see, so it must change the fingerprint too"*;
  `PINNED_FIRST_RANDOM`/`PINNED_SEED_ZERO`).
- **`budget none`**: our deadline preemption uses the rquickjs interrupt
  handler without counting polls — same as pi's in-thread backend
  (`deterministic.test.ts:207` expects `budget none` there).
- **Enforcement**: the fingerprint travels in `SuspendState`; `resume()`
  refuses a mismatch (stronger than pi, which only reports to the host).
- Output/tool-call limits are deliberately **not** in the fingerprint — pi
  excludes them too; they affect output and call counts, not replay identity.

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
- **Host-registered globals** (pi's generic mechanism, `prelude-source.ts:262`):
  the engine accepts named globals from the host with namespace grouping,
  argument spreading on request, sync globals not recorded as calls, and
  async host-answered globals **numbered as calls** (pi
  `deterministic.test.ts:209`, sandbox tests for grouping/spreading). This
  mechanism is how discovery surfaces are wired (OQ6): the host/router layer
  registers `searchTools` / `describeTool` / `describeNamespace` over the
  Space's search index — the engine core itself only knows `ALL_TOOLS`
  (pi `prelude-source.ts:254`). Because async globals are numbered calls,
  OC's counted-search semantics fall out of the uniform call budget
  automatically.
- Budgets per §5.2 enforced inside the sandbox; over-budget emits the pi-style
  error strings.

### 6.2 Per-item status

| Item | Status | Notes |
|---|---|---|
| `console` (total formatter: cycles → `[Circular]`, NaN/Infinity literal, never throws) | **KEEP** | pi/OC have console; codex removes it — rejected here. OC's total-formatting semantics adopted (contract-tested) |
| `searchTools` / `describeTool` / `describeNamespace` | **KEEP — host layer** (OQ6 resolved) | counted against `maxToolCalls`, observed in call records (OC semantics); BM25 non-negative IDF. Registered by the router/server layer over the generic host-globals mechanism — NOT engine bindings (v1's placement dropped); the ServerHandler always wires them, so the MCP surface always has discovery; headless embedders opt in |
| `notify()` (mid-run output injection) | **REJECT** | tied to the rejected cell lifecycle; MCP has no mid-call content streaming — `notifications/progress` carries tokens, not content items, so there is no transport to ride |
| `setTimeout` / `clearTimeout` | **REJECT** | pi/OC sandboxes have no script-facing timers; real-time timers break determinism (D11); virtual-clock timers are no-ops — virtual time advances only on delivered results, so timing is expressed by awaited calls |
| `yield_control()` / cell primitives | **OUT** | tied to the cell lifecycle (§2 schism); rejected for v2 |
| `generatedImage(...)` | **REJECT** | codex provider semantics; MCP generated images are plain image content blocks — `image(result.content[0])` already covers them |

Principle: the script surface is **pi's surface + adopted OC behaviors**
(counted search, total console). Codex-only globals are rejected unless they
map onto our MCP transport — none of the three do.

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

### 7.1 Deferred & dynamic upstreams (pi-exact — RESOLVED, OQ3)

Verified against pi's MCP extension (`extensions/mcp/index.ts`); this corrects
the earlier "startup snapshot catalog" framing, which was wrong for upstream
tools:

1. **Upstream MCP tools never enter the codemode description catalog.** With
   the default `codemode` exposure, MCP tools are kept "out of the model's
   tool declarations and the codemode description: scripts find the tools
   with `searchTools()` and the server instructions with `describeNamespace()`"
   (`index.ts:9-12`, default at `:137`). The budgeted "Nested tools:" catalog
   is for native/direct tools only — for a pure upstream router it is empty.
2. **The server list lives in the system prompt**, in an `mcp_servers` section
   (`:156`): one line per server — `mcp__<server> (codemode): <summary>` —
   summaries ≤250 chars (`:159`), section ≤4,096 chars (`:164`), servers
   sorted by name, overflow collapsed to "… N more servers; find their tools
   with `searchTools()`" (`renderServersSection`, `:186-218`).
3. **Late connection**: per-prompt re-render; when the section changed (e.g. a
   server connected), the updated section is appended to the conversation
   (`:1176-1183`). The codemode tool itself is never redeclared for upstream
   changes. Servers with only codemode tools connect in the background; the
   first prompt waits only for `direct`-tool servers (`:7-8`).
4. **Discovery**: `searchTools()` ranks the live catalog; `describeNamespace`
   returns a server's instructions; calls to a disconnected server's tools
   fail with a clean error. Disconnects surface as section updates at the
   next prompt.
5. **Per-server exposure config** (pi `:120`): `direct` (declared natively,
   excluded from the catalog in `on` mode), `codemode` (default), `deferred`
   (loaded by tool_search, then called directly — this is the set the
   description's `deferred` parameter excludes, `tool.ts:389-395`), `hidden`.

Consequence for the router: no byte-stability requirement on the codemode
description (upstream changes never touch it); the only dynamic surface is the
system-prompt `mcp_servers` section, which hosts already re-render per prompt.

## 8. Naming & upstream ingestion

### 8.1 Canonical names & normalization (RESOLVED, OQ4 — pi `docs/mcp.md:77`)

Two layers, both pi-exact (the MCP layer is stricter and pre-empts the engine
layer):

- **Server names** (config validation): only letters, digits, `_`, `-`. Names
  differing only in `-` vs `_` are **the same server** — a second registration
  is rejected; a `mcp.json` entry overrides an extension-registered one.
- **Tool names**: `mcp__<server>__<tool>`, every character outside letters,
  digits, and `_` replaced by `_`. Tools of one server whose names then
  collide **all get a hash suffix** (not silent overwrite).
- Engine-layer fallback for native tools remains pi's
  `toCodemodeIdentifier` (`identifier.ts:5-12`: first `[A-Za-z_$]`, rest
  `[A-Za-z0-9_$]`, other → `_`, empty → `_`) with dual-key exposure
  (`prelude-source.ts:216-224`: `tools[jsName]` always; canonical alias only
  if free) and dual-spelling resolution (`execute.ts:659-661,706`). For MCP
  tools the stricter layer above means these fallbacks rarely fire.
- Namespace acceptance is four-spelled (pi `docs/mcp.md:204`):
  `describeNamespace()` and `searchTools({namespace})` accept
  `mcp__dev-radius`, `mcp__dev_radius`, `dev-radius`, `dev_radius`.
  Locked by the ported `sandbox.test.ts > tools > exposes tools under
  normalized identifiers` case.

### 8.2 Ingestion

### 8.2 Ingestion & result conversion (pi-exact — RESOLVED, D13)

- **Scope**: tools **and resources-as-tools** aggregate into the Space
  (resources become callable tools participating in codemode/`searchTools`;
  read resources convert to text/images for the model). **Prompts do not
  aggregate.** Tolerated: servers without the `tools` capability (prompts- or
  resources-only) contribute no tool entries (pi `index.ts:387`).
- **Result conversion** (pi `convertMcpResult`, `tools.ts:215-232`): content
  blocks and `structuredContent` are both preserved; the result's `_meta` is
  stripped; truncated output is saved with `fullOutputPath` reported; content
  blocks and `structuredContent` are both preserved; **`isError` resolves —
  scripts receive the full result; only the model-facing output is marked
  `isError: true`** (pi `tools.ts:8-10`: *"MCP errors are error results for
  the model, but scripts still resolve to the result"*). When `isError` and
  the text is empty, a synthesized `MCP tool <server>/<tool> returned an
  error` line is appended. v1's `isError → Err` rejection semantics are
  dropped. (Distinct path, unchanged: **host-delivered** `Err` answers reject
  the script's promise — pi `deterministic.test.ts:209`.)
- Tolerant `structuredContent`: servers may send it without a declared schema
  and it may be any JSON value (pi `tools.ts:140-143`).
- **Annotations**: the four boolean hints `readOnlyHint`, `destructiveHint`,
  `idempotentHint`, `openWorldHint` are forwarded (`tools.ts`
  `ANNOTATION_HINTS`/`toToolAnnotations`); **icons and entry `_meta` are
  stripped** (`resources.ts:53-55`).
- **Client capabilities**: minimal — no sampling/roots/elicitation declared
  toward upstreams (pi declares none in its MCP extension); servers that
  require sampling are refused by the server and simply unusable — documented,
  not worked around.
- Handshake `instructions` auto-attach (v1 behavior, kept).
- **Auth boundary**: the crate consumes **connected peers**; transport and
  OAuth belong to the embedder (pi implements OAuth itself only because it is
  the host opening its own transports — a form-factor difference, not
  semantics).

### 8.3 Upstream lifecycle (pi policy verified, `docs/mcp.md:100` — OQ7 largely resolved)

- Background connect for every enabled server; tools appear on connect; the
  codemode description does not list them, so it does not change when servers
  connect.
- The first prompt waits up to **10 s** only for servers with `direct` tools;
  others are waited for **on demand**: a codemode script waits for the servers
  it names (`mcp__<server>`), and `searchTools()` / `ALL_TOOLS` waits for all.
- HTTP network errors and transient statuses (408, 429, 5xx) retry **twice**;
  a dropped connection reconnects on the next call.
- A changed tool list: new tools added, withdrawn tools become unreachable.
- **Cancellation**: the embedder's abort signal propagates into every upstream
  call (pi `tools.ts:275-278` passes `signal` per call); cancelling a codemode
  run aborts its in-flight upstream calls and frees the worker slot.
- Library API shape (Rust-side) is the remaining sliver of OQ7.

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

### 10.1 MCP protocol baseline (D12 — verified 2026-11)

- **Baseline**: spec `2025-11-25` plus the Final SEPs this design relies on.
  Draft-only features are tracked, not adopted.
- **Tool names (SEP-986, Final)**: SHOULD 1–64 chars, case-sensitive, charset
  `[A-Za-z0-9_./-]`. Our `mcp__<server>__<tool>` (D4) complies; the `mcp__`
  prefix is a host convention the spec does not govern. Because the spec
  allows `.` and `/` in upstream tool names (e.g. `user-profile/update`),
  normalization (§8.1) is load-bearing, not hypothetical.
- **`_meta` key grammar** (spec "General fields", 2025-06-18 → current):
  optional reverse-DNS prefix + name; second label `modelcontextprotocol` or
  `mcp` is reserved. Store-domain selection moves from v1's
  `codemode/storeDomain` to **`io.github.g36maid/storeDomain`** (v1's key
  violates the reverse-DNS SHOULD). SEP-1319 is cited only for what it is —
  the structural decoupling that guarantees `_meta` on every request — not as
  the source of `_meta` semantics.
- **Elicitation**: client capabilities `elicitation: {form, url}`; SEP-1036
  (URL mode) and SEP-1330 (enum schema) are Final. Used by embedders and by
  the future Tasks-based approval flow (D5), not by v2 wire behavior.
- **Tasks**: experimental in 2025-11-25 (SEP-1686), now the
  `io.modelcontextprotocol/tasks` extension (SEP-2663, Final). Not adopted in
  v2 (D5); the codemode tool MAY declare `execution.taskSupport: "forbidden"`
  in v2 so hosts know polling is not available yet.
- **Tracked, not adopted**: draft per-request protocol fields
  (`io.modelcontextprotocol/protocolVersion` etc.), SEP-2567 sessionless
  handles, SEP-2575 stateless guidance, SEP-2577 (deprecating roots,
  sampling, logging — relevant to which upstream capabilities we forward).

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

v1's tests are **not ported** to v2 (the oracle replaces them; see OQ1). Where
v1 code is consulted during the rewrite, only its QuickJS workarounds and
verified bug fixes (non-negative BM25 IDF, counted search, total console
formatting) carry over — as reference, not as authority.

### 11.5 Rewrite sequence

1. **Harness first**: port pi's sandbox API shape (`types.ts`: `execute`,
   `start`/`deliver`, outcomes, `timeoutMs`, abort) to the Rust engine trait —
   the public API is derived from pi's protocol, not from v1's `RunContext`.
2. Port `sandbox.test.ts` (55) — engine implementation makes them pass;
   prelude/bindings written as clean JS source + hash (pi's
   `prelude-source.ts` shape), not Rust format strings.
3. Port `deterministic.test.ts` (24), then `declarations.test.ts` (19 — full
   `schemaToType` renderer).
4. Port `portable`/`source`; document `worker-lifetime` cases `N/A` with
   reasons where Rust threading differs.
5. Port OC's four suites (§11.3).
6. Upstream ingestion + naming (§8), catalog description (§7), server handler
   (§10) with real-MCP fixtures (§11.2).

## 12. Out of scope (v2)

- Wire-level approval / elicitation (engine capability remains; D5).
- Binary packaging, config files, stdio/HTTP server wiring (D2).
- Cell/wait persistent-cell lifecycle (codex model) — rejected for v2 (§2).
- A V8 engine implementation — the Engine trait permits one; not planned.

## 13. Open questions

| # | Question | Options on the table |
|---|---|---|
| OQ1 | ~~v1 disposition~~ **RESOLVED** | **clean rewrite from this spec**, oracle-first: the ported pi suite (§11.1) is written before the engine that must pass it. v1 stays on main as reference material (QuickJS workarounds, verified fixes) until v2 passes the ported oracle suite, then archived; v1's hand-written tests are not ported (§11.4) |
| OQ2 | ~~Crate split~~ **RESOLVED** | keep v1's two crates (core contracts / quickjs engine + server handler); revisit only if the server layer grows worth independent release |
| OQ3 | ~~Deferred listing policy~~ **RESOLVED — pi-exact** | upstream MCP tools never listed in the description (default `codemode` exposure); server list = system-prompt `mcp_servers` section (≤4,096 chars, ≤250/server), re-rendered per prompt, appended on change; discovery via `searchTools` + `describeNamespace`; codemode tool never redeclared for upstream changes; per-server exposure config `direct/codemode/deferred/hidden` (§7.1) |
| OQ4 | ~~Name normalization~~ **RESOLVED — pi-exact, two layers** | server names: config-valid `[letters/digits/_/-]`, `-`≈`_` equivalence (same server, second rejected); tool names: non-alnum → `_`, colliding tools get hash suffixes (`docs/mcp.md:77`); engine-layer identifier fallback unchanged; four-spelling namespace acceptance (`:204`) (§8.1) |
| OQ5 | ~~Fingerprint composition~~ **RESOLVED — pi-exact** | pi's exact slot list: engine versions + FNV-1a prelude hash + `DETERMINISTIC_VERSION` (pinned-value-test discipline) + stack/heap from `Budget` (`max_stack_size` knob added) + `budget none`; enforced in `resume()` via `SuspendState` (§5.5) |
| OQ6 | ~~searchTools layer placement~~ **RESOLVED — pi-exact (B)** | engine provides only the generic host-globals mechanism (grouping/spreading/numbering); searchTools/describeTool/describeNamespace are router-layer registrations over the Space index; engine core keeps only `ALL_TOOLS` (§6.1, §6.2) |
| OQ7 | Upstream lifecycle API | **policy resolved** (pi `docs/mcp.md:100`: background connect, 10 s direct-wait, on-demand waits, retry 408/429/5xx ×2, reconnect on next call, tool-list change = add/unreachable — §8.3); remaining: Rust API shape |
| OQ8 | ~~codex-only globals~~ **RESOLVED — all three rejected** | `notify()` (no transport, tied to rejected lifecycle), `setTimeout` (breaks determinism / no-op on virtual clock), `generatedImage` (subsumed by `image()`); principle: script surface = pi + adopted OC behaviors (§6.2) |
| OQ9 | ~~In-flight call limiter~~ **RESOLVED — none, pi-exact** | nested calls unlimited within a run; premise corrected: pi's `createLimiter` guards model calls only, not nested calls (§5.1); upstream pressure belongs to upstreams/embedders; `Budget.max_in_flight_calls` stays an additive future option (script-invisible) |
