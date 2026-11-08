//! The script-visible globals: the `tools` proxy tree, `ALL_TOOLS`, the
//! output helpers, `store`/`load`, `exit()`, and `searchTools`/
//! `describeTool`/`describeNamespace`.
//!
//! Everything crosses the VM boundary as JSON strings through narrow
//! `__`-prefixed functions bound here. The tree is built from the run's
//! tool paths: a path with dots nests (OpenCode's shape), a one-segment
//! path is a flat leaf (pi's shape) — one proxy implementation, both forms.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use rmcp_codemode::{
    Budget, NamespaceMeta, OutputItem, Space, ToolHandle, Value, flat_identifier,
    identity::unknown_member_message, normalize_identifier, signature::ts_declaration,
};
use rquickjs::prelude::{Async as AsyncFn, Func};
use serde_json::json;

/// What a dispatched call does — provided by the run module (one-shot runs
/// execute inline; suspending runs yield to the host; replay answers from
/// the recording).
pub(crate) type AsyncCallSink = Arc<
    dyn Fn(ToolHandle, Value) -> futures::future::BoxFuture<'static, Result<Value, String>>
        + Send
        + Sync,
>;

/// Bind every script-facing global.
///
/// `pending_call` is the sink the dispatcher yields calls to (one-shot runs
/// execute them inline; suspending runs forward them to the host).
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)] // one setup, many globals
pub(crate) fn setup(
    ctx: &rquickjs::Ctx<'_>,
    space: &Space,
    namespaces: &BTreeMap<String, NamespaceMeta>,
    budget: &Budget,
    pending: &Arc<Mutex<Vec<OutputItem>>>,
    calls: &Arc<Mutex<Vec<rmcp_codemode::CallRecord>>>,
    call_counter: &Arc<AtomicU64>,
    exit_requested: &Arc<AtomicBool>,
    dispatcher_sink: &AsyncCallSink,
    initial_store: &Arc<Mutex<BTreeMap<String, Value>>>,
    pending_store: &Arc<Mutex<BTreeMap<String, Value>>>,
    deterministic: &Option<Arc<Mutex<crate::run::VirtualClock>>>,
    seed: u32,
) -> Result<(), rquickjs::Error> {
    let globals = ctx.globals();

    // ---- output emitters: return an error message (empty = ok) on caps or
    // after exit; the JS leaf throws it. ----
    for (name, console) in [("__emitText", false), ("__emitConsole", true)] {
        let pending = Arc::clone(pending);
        let exit_requested = Arc::clone(exit_requested);
        let budget = budget.clone();
        globals.set(
            name,
            Func::from(move |text: String| -> String {
                if exit_requested.load(Ordering::Relaxed) {
                    return "script has exited".to_owned();
                }
                let mut guard = pending.lock().unwrap();
                if guard.len() >= budget.max_output_items {
                    return format!(
                        "output limit exceeded: more than {} output calls",
                        budget.max_output_items
                    );
                }
                if guard.iter().map(item_len).sum::<usize>() + text.len() > budget.max_output_chars
                {
                    return format!(
                        "output limit exceeded: more than {} characters of output",
                        budget.max_output_chars
                    );
                }
                guard.push(if console {
                    OutputItem::Console { text }
                } else {
                    OutputItem::Text { text }
                });
                String::new()
            }),
        )?;
    }

    // ---- image emitter: parse a data URI or bare base64 (assumed PNG). ----
    {
        let pending = Arc::clone(pending);
        let exit_requested = Arc::clone(exit_requested);
        let budget = budget.clone();
        globals.set(
            "__emitImage",
            Func::from(move |value: String| -> String {
                if exit_requested.load(Ordering::Relaxed) {
                    return "script has exited".to_owned();
                }
                let Some((mime_type, data)) = parse_image_input(&value) else {
                    return "image() expects a base64 data URI like data:image/png;base64,..."
                        .to_owned();
                };
                let mut guard = pending.lock().unwrap();
                if guard.len() >= budget.max_output_items {
                    return format!(
                        "output limit exceeded: more than {} output calls",
                        budget.max_output_items
                    );
                }
                if guard.iter().map(item_len).sum::<usize>() + data.len() > budget.max_output_chars
                {
                    return format!(
                        "output limit exceeded: more than {} characters of output",
                        budget.max_output_chars
                    );
                }
                guard.push(OutputItem::Image { data, mime_type });
                String::new()
            }),
        )?;
    }

    // ---- tool dispatcher: (path, argsJson) -> envelope JSON. One-shot runs
    // execute inline; suspending runs forward through `pending_call`. ----
    {
        let calls = Arc::clone(calls);
        let counter = Arc::clone(call_counter);
        let exit_requested = Arc::clone(exit_requested);
        let space = space.clone();
        let dispatcher_sink = dispatcher_sink.clone();
        let max_calls = budget.max_tool_calls;
        let dispatcher = move |path: String, args_json: String| {
            let calls = Arc::clone(&calls);
            let counter = Arc::clone(&counter);
            let exit_requested = Arc::clone(&exit_requested);
            let space = space.clone();
            let dispatcher_sink = dispatcher_sink.clone();
            async move {
                if exit_requested.load(Ordering::Relaxed) {
                    return Ok::<String, rquickjs::Error>(
                        json!({ "ok": false, "error": "script has exited" }).to_string(),
                    );
                }
                let Some(tool) = space.resolve_tool(&path) else {
                    let names: Vec<String> = space.entries().map(|t| t.canonical()).collect();
                    return Ok(json!({
                        "ok": false,
                        "error": unknown_member_message(
                            "tools",
                            &path,
                            &names,
                            "ALL_TOOLS lists every tool; searchTools(query) finds tools by topic.",
                        )
                    })
                    .to_string());
                };
                if args_json != "null" && serde_json::from_str::<Value>(&args_json).is_err() {
                    return Ok(json!({ "ok": false, "error": format!("Invalid arguments for tool '{path}': not valid JSON") }).to_string());
                }
                let args: Value = serde_json::from_str(&args_json).unwrap_or(Value::Null);
                if let Some(max) = max_calls
                    && counter.load(Ordering::Relaxed) as usize >= max
                {
                    return Ok(json!({ "ok": false, "error": format!("Execution exceeded its tool-call limit of {max}.") }).to_string());
                }
                counter.fetch_add(1, Ordering::Relaxed);
                let started = std::time::Instant::now();
                let outcome = (dispatcher_sink)(tool.clone(), args).await;
                let duration = started.elapsed().as_millis() as u64;
                let status = match &outcome {
                    Ok(_) => rmcp_codemode::CallStatus::Ok,
                    Err(_) => rmcp_codemode::CallStatus::Error,
                };
                calls
                    .lock()
                    .unwrap()
                    .push(rmcp_codemode::CallRecord::finished(
                        tool.canonical(),
                        status,
                        duration,
                    ));
                Ok(match outcome {
                    Ok(value) => json!({ "ok": true, "value": value }).to_string(),
                    Err(message) => json!({ "ok": false, "error": message }).to_string(),
                })
            }
        };
        globals.set("__callTool", Func::from(AsyncFn(dispatcher)))?;
    }

    // ---- discovery bridges ----
    {
        let space = space.clone();
        let namespaces = namespaces.clone();
        globals.set(
            "__searchTools",
            Func::from(move |query: String, opts_json: String| -> String {
                search_tools_response(&space, &namespaces, &query, &opts_json)
            }),
        )?;
    }
    {
        let space = space.clone();
        globals.set(
            "__describeTool",
            Func::from(move |name: String| -> String { describe_tool_response(&space, &name) }),
        )?;
    }
    {
        let space = space.clone();
        let namespaces = namespaces.clone();
        globals.set(
            "__describeNamespace",
            Func::from(move |name: String| -> String {
                describe_namespace_response(&space, &namespaces, &name)
            }),
        )?;
    }

    // ---- store: reads see the run's initial snapshot; writes buffer in
    // pending_store and commit on success (pi's split) ----
    {
        let pending_store = Arc::clone(pending_store);
        let exit_requested = Arc::clone(exit_requested);
        globals.set(
            "__storeValue",
            Func::from(
                move |key: String, value_json: String, is_delete: bool| -> String {
                    if exit_requested.load(Ordering::Relaxed) {
                        return "script has exited".to_owned();
                    }
                    let mut pending = pending_store.lock().unwrap();
                    if is_delete {
                        pending.insert(key, Value::Null);
                        return String::new();
                    }
                    if value_json.len() > rmcp_codemode::STORE_MAX_VALUE_CHARS {
                        return format!(
                            "store limit exceeded: one value may hold at most {} characters of JSON",
                            rmcp_codemode::STORE_MAX_VALUE_CHARS
                        );
                    }
                    let Ok(value) = serde_json::from_str::<Value>(&value_json) else {
                        return "store() failed to encode the value".to_owned();
                    };
                    let total: usize = pending
                        .values()
                        .map(|v| serde_json::to_string(v).map_or(0, |s| s.len()))
                        .sum();
                    if total > rmcp_codemode::STORE_MAX_TOTAL_CHARS {
                        return format!(
                            "store limit exceeded: all values together may hold at most {} characters of JSON",
                            rmcp_codemode::STORE_MAX_TOTAL_CHARS
                        );
                    }
                    pending.insert(key, value);
                    String::new()
                },
            ),
        )?;
    }
    {
        let initial_store = Arc::clone(initial_store);
        let pending_store = Arc::clone(pending_store);
        globals.set(
            "__loadValue",
            Func::from(move |key: String| -> String {
                if let Some(value) = pending_store.lock().unwrap().get(&key) {
                    return serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned());
                }
                match initial_store.lock().unwrap().get(&key) {
                    Some(value) => {
                        serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned())
                    }
                    None => "null".to_owned(),
                }
            }),
        )?;
    }

    // ---- exit ----
    {
        let exit_requested = Arc::clone(exit_requested);
        globals.set(
            "__requestExit",
            Func::from(move || exit_requested.store(true, Ordering::Relaxed)),
        )?;
    }

    if let Some(clock) = deterministic {
        bind_deterministic_globals(ctx, clock, seed)?;
    }

    bind_script_globals(ctx, space, namespaces)
}

/// Deterministic mode (pi's replay VM): `Date` reads the virtual clock,
/// `Math.random` draws from a PRNG over the seed, and `WeakRef` /
/// `FinalizationRegistry` do not exist (their scheduling is unobservable
/// and would break replay).
fn bind_deterministic_globals(
    ctx: &rquickjs::Ctx<'_>,
    clock: &Arc<Mutex<crate::run::VirtualClock>>,
    seed: u32,
) -> Result<(), rquickjs::Error> {
    let globals = ctx.globals();
    {
        let clock = Arc::clone(clock);
        globals.set(
            "__virtualNow",
            Func::from(move || clock.lock().unwrap().now_ms),
        )?;
    }
    ctx.eval::<(), _>(
        r#"
globalThis.__rngState = {__seed} >>> 0;
// mulberry32 (pi's PRNG): one 32-bit draw per call, [0, 1).
globalThis.Math.random = () => {{
    __rngState = (__rngState + 0x6d2b79f5) >>> 0;
    let t = __rngState;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}};
// `Date` reads the virtual clock: Date.now() and new Date() with no
// arguments; every other constructor form passes through.
const __RealDate = Date;
const __VDate = function (...args) {{
    if (new.target === undefined) return __RealDate(globalThis.__virtualNow());
    if (args.length === 0) return Reflect.construct(__RealDate, [globalThis.__virtualNow()]);
    return Reflect.construct(__RealDate, args);
}};
__VDate.prototype = __RealDate.prototype;
__VDate.now = () => globalThis.__virtualNow();
__VDate.parse = __RealDate.parse;
__VDate.UTC = __RealDate.UTC;
globalThis.Date = __VDate;
// Unobservable scheduling has no place in a replayable run (pi removes
// these in deterministic mode).
delete globalThis.WeakRef;
delete globalThis.FinalizationRegistry;
"#
        .replace("{__seed}", &seed.to_string()),
    )?;
    Ok(())
}

fn item_len(item: &OutputItem) -> usize {
    rmcp_codemode::item_chars(item)
}

/// Parse an `image()` argument: a `data:<mime>;base64,<data>` URI, or bare
/// base64 (assumed PNG).
fn parse_image_input(value: &str) -> Option<(String, String)> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("data:") {
        let (metadata, data) = rest.split_once(',')?;
        let mime_type = metadata
            .strip_suffix(";base64")
            .filter(|m| !m.is_empty())?
            .to_owned();
        (!data.is_empty()).then(|| (mime_type, data.to_owned()))
    } else if !value.is_empty() {
        Some(("image/png".to_owned(), value.to_owned()))
    } else {
        None
    }
}

/// The `searchTools` response: OpenCode's `{ items, remaining, next }` over
/// a BM25 ranking (pi's default limit 8). Scoped by namespace, resolved
/// through aliases.
fn search_tools_response(
    space: &Space,
    namespaces: &BTreeMap<String, NamespaceMeta>,
    query: &str,
    opts_json: &str,
) -> String {
    let options: Value = serde_json::from_str(opts_json).unwrap_or(json!({}));
    let limit = options
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(8);
    let offset = options
        .get("offset")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(0);
    let owned_scoped: Vec<(String, Option<String>)> =
        match options.get("namespace").and_then(Value::as_str) {
            Some(name) => match space.resolve_namespace(name) {
                Some(key) => space
                    .namespace_members(&key)
                    .into_iter()
                    .map(|tool| {
                        (
                            tool.canonical(),
                            Some(tool.description.clone().unwrap_or_default()),
                        )
                    })
                    .collect(),
                None => Vec::new(),
            },
            None => space
                .entries()
                .map(|tool| {
                    (
                        tool.canonical(),
                        Some(tool.description.clone().unwrap_or_default()),
                    )
                })
                .collect(),
        };
    let scoped: Vec<(&str, Option<&str>)> = owned_scoped
        .iter()
        .map(|(path, description)| (path.as_str(), description.as_deref()))
        .collect();
    if scoped.is_empty() {
        return json!({ "items": [], "remaining": 0, "next": null }).to_string();
    }
    let mut index = rmcp_codemode::search_shim::SearchIndex::default();
    // Namespace descriptions participate for every descendant (OpenCode).
    let mut extra_by_path: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for tool in space.entries() {
        let mut extra = Vec::new();
        for depth in 1..tool.segments.len() {
            let key = tool.segments[..depth].join(".");
            if let Some(meta) = namespaces.get(&key) {
                if let Some(description) = &meta.description {
                    extra.push(description.clone());
                }
                if let Some(instructions) = &meta.instructions {
                    extra.push(instructions.clone());
                }
            }
        }
        extra_by_path.insert(tool.canonical(), extra);
    }
    for (path, description) in &scoped {
        index.add(path, *description, &extra_by_path[*path]);
    }
    let ranked = index.search(query, scoped.len());
    let page = ranked.iter().skip(offset).take(limit);
    let items: Vec<Value> = page
        .map(|hit| json!({ "name": hit.path, "description": hit.description }))
        .collect();
    let remaining = ranked.len().saturating_sub(offset + items.len());
    let next = (remaining > 0)
        .then(|| json!(offset + items.len()))
        .unwrap_or(json!(null));
    json!({ "items": items, "remaining": remaining, "next": next }).to_string()
}

/// The `describeTool` response: description + declaration, or `null`.
fn describe_tool_response(space: &Space, name: &str) -> String {
    let Some(tool) = space.resolve_tool(name) else {
        return "null".to_owned();
    };
    let response = json!({
        "name": tool.canonical(),
        "description": tool.description.clone().unwrap_or_default(),
        "declaration": ts_declaration(
            &tool.canonical(),
            tool.description.as_deref(),
            tool.schema.input.as_ref(),
            tool.schema.output.as_ref(),
        ),
    });
    serde_json::to_string(&response).unwrap_or_else(|_| "null".to_owned())
}

/// The `describeNamespace` response (pi's shape), resolving through
/// aliases; `null` when the name resolves to no namespace with members.
fn describe_namespace_response(
    space: &Space,
    namespaces: &BTreeMap<String, NamespaceMeta>,
    name: &str,
) -> String {
    let Some(key) = space.resolve_namespace(name) else {
        return "null".to_owned();
    };
    let members = space.namespace_members(&key);
    if members.is_empty() {
        return "null".to_owned();
    }
    let meta = namespaces.get(&key);
    let tools_json: Vec<Value> = members
        .iter()
        .map(|tool| {
            json!({
                "name": tool.canonical(),
                "description": tool.description.clone().unwrap_or_default(),
                "declaration": ts_declaration(
                    &tool.canonical(),
                    tool.description.as_deref(),
                    tool.schema.input.as_ref(),
                    tool.schema.output.as_ref(),
                ),
            })
        })
        .collect();
    let mut response = serde_json::Map::new();
    response.insert("name".to_owned(), json!(key));
    if let Some(description) = meta.and_then(|m| m.description.as_deref()) {
        response.insert("description".to_owned(), json!(description));
    }
    if let Some(instructions) = meta.and_then(|m| m.instructions.as_deref()) {
        response.insert("instructions".to_owned(), json!(instructions));
    }
    response.insert("tools".to_owned(), json!(tools_json));
    serde_json::to_string(&Value::Object(response)).unwrap_or_else(|_| "null".to_owned())
}

/// The JS-side globals: the tools proxy tree, `ALL_TOOLS`, output helpers,
/// `store`/`load`, `exit()`, discovery wrappers. Idempotent — `execute_with`
/// may run this once per VM.
fn bind_script_globals(
    ctx: &rquickjs::Ctx<'_>,
    space: &Space,
    namespaces: &BTreeMap<String, NamespaceMeta>,
) -> Result<(), rquickjs::Error> {
    // Leaf paths in every spelling the script may use: canonical, the flat
    // identifier projection (pi), and the plain normalized identifier
    // (`my-tool` and `my_tool` both call the same tool).
    let mut paths: Vec<String> = Vec::new();
    for tool in space.entries() {
        let canonical = tool.canonical();
        paths.push(canonical.clone());
        let flat = flat_identifier(&tool.segments);
        if flat != canonical {
            paths.push(flat.clone());
        }
        let normalized = normalize_identifier(&canonical);
        if normalized != canonical && normalized != flat {
            paths.push(normalized);
        }
    }
    let tools_spec = serde_json::to_string(&paths).unwrap_or_else(|_| "[]".to_owned());
    let all_tools: Vec<Value> = space
        .entries()
        .map(|tool| {
            json!({
                "name": rmcp_codemode::flat_identifier(&tool.segments),
                "description": tool.description.clone().unwrap_or_default(),
            })
        })
        .collect();
    let all_tools_json = serde_json::to_string(&all_tools).unwrap_or_else(|_| "[]".to_owned());

    let aliases: BTreeMap<String, Vec<String>> = namespaces
        .iter()
        .map(|(key, meta)| (key.clone(), meta.aliases.clone()))
        .collect();
    let aliases_json = serde_json::to_string(&aliases).unwrap_or_else(|_| "{}".to_owned());

    let setup = format!(
        r#"
globalThis.__leafPaths = new Set(JSON.parse('{tools_spec}'));
globalThis.__nsAliases = JSON.parse('{aliases_json}');
globalThis.ALL_TOOLS = JSON.parse('{all_tools_json}');

globalThis.__makeLeaf = (path) => async (args) => {{
    const envelope = JSON.parse(await globalThis.__callTool(path, JSON.stringify(args === undefined ? null : args)));
    if (!envelope.ok) throw new Error(envelope.error);
    return envelope.value;
}};
globalThis.__isNamespace = (prefix) => {{
    for (const path of globalThis.__leafPaths) if (path.startsWith(prefix + ".")) return true;
    return false;
}};
globalThis.__childSegments = (prefix) => {{
    const out = new Set();
    const at = prefix === "" ? "" : prefix + ".";
    for (const path of globalThis.__leafPaths) {{
        if (!path.startsWith(at)) continue;
        out.add(path.slice(at.length).split(".")[0]);
    }}
    return out;
}};
globalThis.__memberPath = (prefix, prop) => (prefix ? prefix + "." + prop : prop);
globalThis.__reserved = new Set(["then", "toString", "valueOf", "toJSON", "call", "apply", "bind"]);
globalThis.__resolvePath = (path) => {{
    if (globalThis.__leafPaths.has(path)) return __makeLeaf(path);
    if (__isNamespace(path)) return __makeNamespace(path);
    return __makeLeaf(path);
}};
globalThis.__objectHandler = (prefix, label) => ({{
    get(_target, prop) {{
        if (typeof prop !== "string") return undefined;
        if (__reserved.has(prop) && !globalThis.__leafPaths.has(__memberPath(prefix, prop))) return undefined;
        return __resolvePath(__memberPath(prefix, prop));
    }},
    // pi's guard: reading a member that does not exist names the close
    // matches instead of failing later with "not a function".
    has(_target, prop) {{
        if (typeof prop !== "string") return Reflect.has(_target, prop);
        const path = __memberPath(prefix, prop);
        return globalThis.__leafPaths.has(path) || __isNamespace(path) || Reflect.has(_target, prop);
    }},
    ownKeys(target) {{
        const segs = __childSegments(prefix);
        return [...segs, ...Reflect.ownKeys(target)];
    }},
    getOwnPropertyDescriptor(target, prop) {{
        if (typeof prop === "string" && __childSegments(prefix).has(prop)) {{
            return {{ value: __resolvePath(__memberPath(prefix, prop)), writable: true, enumerable: true, configurable: true }};
        }}
        return Reflect.getOwnPropertyDescriptor(target, prop);
    }},
}});
// Unknown-member guard: resolve first; only then name close matches
// (separator-insensitive identity, then two-way substring, max five) or the
// availability list, and always the in-check hint (pi's prelude).
globalThis.__guarded = (target, label, names, hint) => new Proxy(target, {{
    get(object, prop, receiver) {{
        if (typeof prop !== "string" || prop in object || prop in Object.prototype || prop === "then" || prop === "toJSON") {{
            return Reflect.get(object, prop, receiver);
        }}
        throw new Error(__unknownMember(label, prop, names, hint));
    }},
}});
globalThis.__makeNamespace = function (prefix) {{
    return new Proxy(function () {{}}, {{
        ...__objectHandler(prefix, prefix),
        apply(_target, _this, args) {{
            if (globalThis.__leafPaths.has(prefix)) return __makeLeaf(prefix).apply(null, args);
            throw new Error(`Tool '${{prefix}}' is not callable`);
        }},
    }});
}};
globalThis.__makeObjectNamespace = (prefix) => new Proxy({{}}, __objectHandler(prefix, prefix));
globalThis.tools = __guarded(__makeObjectNamespace(""), "tools",
    JSON.stringify([...globalThis.__leafPaths]),
    "ALL_TOOLS lists every tool; searchTools(query) finds tools by topic.");

globalThis.text = (value) => {{
    const err = globalThis.__emitText(typeof value === "string" ? value : JSON.stringify(value));
    if (err) throw new Error(err);
}};
globalThis.console = {{
    log: (...parts) => {{
        const err = globalThis.__emitConsole(parts.map((p) => (typeof p === "string" ? p : JSON.stringify(p))).join(" "));
        if (err) throw new Error(err);
    }},
}};
globalThis.console.info = globalThis.console.log;
globalThis.console.warn = globalThis.console.log;
globalThis.console.error = globalThis.console.log;
globalThis.console.debug = globalThis.console.log;
globalThis.image = (value) => {{
    const err = globalThis.__emitImage(typeof value === "string" ? value : "");
    if (err) throw new Error(err);
}};
globalThis.store = (key, value) => {{
    const err = globalThis.__storeValue(String(key), value === undefined ? "null" : JSON.stringify(value), value === undefined);
    if (err) throw new Error(err);
}};
globalThis.load = (key) => {{
    const raw = globalThis.__loadValue(String(key));
    return raw === "null" ? undefined : JSON.parse(raw);
}};
globalThis.exit = () => {{
    globalThis.__requestExit();
    throw {{ __codemode_exit: true }};
}};
globalThis.searchTools = async (query, options) =>
    JSON.parse(globalThis.__searchTools(query === undefined ? "" : String(query), JSON.stringify(options ?? {{}})));
globalThis.describeTool = async (name) => {{
    const raw = globalThis.__describeTool(String(name));
    return raw === "null" ? undefined : JSON.parse(raw);
}};
globalThis.describeNamespace = async (name) => {{
    const raw = globalThis.__describeNamespace(String(name));
    return raw === "null" ? undefined : JSON.parse(raw);
}};
"#,
        tools_spec = tools_spec,
        all_tools_json = all_tools_json,
        aliases_json = aliases_json,
    );
    // Unknown-member helper is host-side (needs the name list) — bind it as
    // a native function.
    ctx.globals().set(
        "__unknownMember",
        Func::from(
            |label: String, property: String, names_json: String, hint: String| -> String {
                let names: Vec<String> = serde_json::from_str(&names_json).unwrap_or_default();
                unknown_member_message(&label, &property, &names, &hint)
            },
        ),
    )?;
    if let Err(error) = ctx.eval::<(), _>(setup.as_bytes()) {
        // Setup is engine-authored; surface the real message.
        let detail = if matches!(error, rquickjs::Error::Exception) {
            let caught = ctx.catch();
            crate::run::describe_caught(ctx, rquickjs::CaughtError::Value(caught))
        } else {
            error.to_string()
        };
        return Err(rquickjs::Error::FromJs {
            from: "setup",
            to: "globals",
            message: Some(detail),
        });
    }
    Ok(())
}
