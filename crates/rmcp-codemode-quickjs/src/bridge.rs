//! The VM bridge: how script globals reach host capabilities.
//!
//! Everything crosses the VM boundary as JSON strings through narrow
//! functions bound here — output emitters, the store, the tool dispatcher,
//! and the discovery primitives (`searchTools` / `describeTool` /
//! `describeNamespace`).

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Instant,
};

use rmcp_codemode::{
    MAX_OUTPUT_CALLS, MAX_OUTPUT_CHARS, OutputItem, SearchIndex, ToolCall, normalize_identifier,
    parse_image_input, ts_declaration, unknown_tool_message,
};
use rquickjs::{
    CatchResultExt,
    prelude::{Async as AsyncFn, Func},
};
use serde_json::{Value, json};

use crate::{
    sandbox::{InFlightGuard, OutputBuffer},
    types::{
        STORE_MAX_TOTAL_CHARS, STORE_MAX_VALUE_CHARS, SandboxError, SandboxTool, Store,
        rquickjs_error,
    },
};

/// Bind `__emitText`, `__emitConsole`, `__emitImage`, `__storeValue`,
/// `__loadValue`, `__callTool`, `__searchTools`, `__describeTool`, and
/// `__describeNamespace`, and the script-facing globals (`tools`, `text`,
/// `console`, `image`, `store`, `load`, `ALL_TOOLS`, `searchTools`,
/// `describeTool`, `describeNamespace`, `exit`).
#[allow(clippy::too_many_arguments)] // bridge wiring, all needed
pub(crate) fn setup_globals<'js>(
    ctx: &rquickjs::Ctx<'js>,
    output: Arc<Mutex<OutputBuffer>>,
    calls: Arc<Mutex<Vec<ToolCall>>>,
    tools: Arc<BTreeMap<String, SandboxTool>>,
    in_flight: Arc<AtomicUsize>,
    exit_requested: Arc<AtomicBool>,
    pending_store: Arc<Mutex<BTreeMap<String, Value>>>,
    committed_store: Option<Store>,
    namespace_infos: Arc<BTreeMap<String, rmcp_codemode::NamespaceInfo>>,
) -> Result<(), SandboxError> {
    let globals = ctx.globals();

    // ---- output emitters: return an error message (empty = ok) when an
    // output cap is hit or the script has exited; the JS leaf throws it. ----
    {
        let output = Arc::clone(&output);
        let exit_requested = Arc::clone(&exit_requested);
        globals
            .set(
                "__emitText",
                Func::from(move |text: String| -> String {
                    if exit_requested.load(Ordering::Relaxed) {
                        return "script has exited".to_owned();
                    }
                    let mut guard = output.lock().unwrap();
                    if guard.items.len() >= MAX_OUTPUT_CALLS {
                        return format!(
                            "output limit exceeded: more than {MAX_OUTPUT_CALLS} text/console calls"
                        );
                    }
                    if guard.chars + text.len() > MAX_OUTPUT_CHARS {
                        return format!(
                            "output limit exceeded: more than {MAX_OUTPUT_CHARS} characters of output"
                        );
                    }
                    guard.push(OutputItem::Text(text));
                    String::new()
                }),
            )
            .map_err(rquickjs_error)?;
    }
    {
        let output = Arc::clone(&output);
        let exit_requested = Arc::clone(&exit_requested);
        globals
            .set(
                "__emitConsole",
                Func::from(move |line: String| -> String {
                    if exit_requested.load(Ordering::Relaxed) {
                        return "script has exited".to_owned();
                    }
                    let mut guard = output.lock().unwrap();
                    if guard.items.len() >= MAX_OUTPUT_CALLS {
                        return format!(
                            "output limit exceeded: more than {MAX_OUTPUT_CALLS} text/console calls"
                        );
                    }
                    if guard.chars + line.len() > MAX_OUTPUT_CHARS {
                        return format!(
                            "output limit exceeded: more than {MAX_OUTPUT_CHARS} characters of output"
                        );
                    }
                    guard.push(OutputItem::Console(line));
                    String::new()
                }),
            )
            .map_err(rquickjs_error)?;
    }

    // ---- tool dispatcher: (path, argsJson) -> envelope JSON string; the JS
    // leaf throws `new Error(envelope.error)` when the call failed, so the
    // script sees clean messages (pi's semantics). Lookup order: exact path,
    // then normalized identifier aliases (pi's `my-tool` ↔ `tools.my_tool`),
    // then a close-match suggestion (pi's "Did you mean"). ----
    {
        let calls = Arc::clone(&calls);
        let tools = Arc::clone(&tools);
        let in_flight = Arc::clone(&in_flight);
        let exit_requested = Arc::clone(&exit_requested);
        let call_tool = move |path: String, args_json: String| {
            let calls = Arc::clone(&calls);
            let tools = Arc::clone(&tools);
            let in_flight = Arc::clone(&in_flight);
            let exit_requested = Arc::clone(&exit_requested);
            async move {
                let dispatch = async |calls: Arc<Mutex<Vec<ToolCall>>>,
                                      tools: Arc<BTreeMap<String, SandboxTool>>,
                                      in_flight: Arc<AtomicUsize>| {
                    if exit_requested.load(Ordering::Relaxed) {
                        return Ok::<String, rquickjs::Error>(
                            json!({ "ok": false, "error": "script has exited" }).to_string(),
                        );
                    }
                    // Identifier aliases, first registration wins on collision
                    // (pi's semantics: `tools.my_tool` reaches `my-tool`).
                    let canonical = if tools.contains_key(&path) {
                        Some(path.clone())
                    } else {
                        tools
                            .keys()
                            .find(|known| normalize_identifier(known) == path)
                            .cloned()
                    };
                    let Some(tool_path) = canonical else {
                        return Ok::<String, rquickjs::Error>(
                            json!({ "ok": false, "error": unknown_tool_message(&path, &tools.keys().cloned().collect::<Vec<_>>()) })
                                .to_string(),
                        );
                    };
                    let path = tool_path;
                    let Some(tool) = tools.get(&path) else {
                        return Ok(
                            json!({ "ok": false, "error": unknown_tool_message(&path, &tools.keys().cloned().collect::<Vec<_>>()) })
                                .to_string(),
                        );
                    };
                    // Validate the argument shape only when the script sent one;
                    // `JSON.stringify(undefined)` arrives as the string "undefined".
                    if args_json != "null" && serde_json::from_str::<Value>(&args_json).is_err() {
                        return Ok(json!({ "ok": false, "error": format!("Invalid arguments for tool '{path}': not valid JSON") }).to_string());
                    }
                    // The in-flight counter powers never-settling detection:
                    // while any upstream call is pending, waiting is legal.
                    in_flight.fetch_add(1, Ordering::Relaxed);
                    let _guard = InFlightGuard(&in_flight);
                    let started = Instant::now();
                    let result = (tool.execute)(args_json.clone()).await;
                    calls.lock().unwrap().push(ToolCall {
                        name: path.clone(),
                        duration_ms: started.elapsed().as_millis(),
                    });
                    Ok(match result {
                        Ok(value) => match serde_json::from_str::<Value>(&value) {
                            Ok(parsed) => json!({ "ok": true, "value": parsed }).to_string(),
                            Err(e) => json!({ "ok": false, "error": format!("Tool '{path}' returned invalid JSON: {e}") }).to_string(),
                        },
                        Err(message) => json!({ "ok": false, "error": format!("{path}: {message}") }).to_string(),
                    })
                };
                dispatch(calls, tools, in_flight).await
            }
        };
        globals
            .set("__callTool", Func::from(AsyncFn(call_tool)))
            .map_err(rquickjs_error)?;
    }

    // ---- image emitter: parses a data URI (`data:<mime>;base64,<data>`) or
    // bare base64 (assumed PNG) into an image output item. ----
    {
        let output = Arc::clone(&output);
        let exit_requested = Arc::clone(&exit_requested);
        globals
            .set(
                "__emitImage",
                Func::from(move |value: String| -> String {
                    if exit_requested.load(Ordering::Relaxed) {
                        return "script has exited".to_owned();
                    }
                    let Some((mime_type, data)) = parse_image_input(&value) else {
                        return "image() expects a base64 data URI like data:image/png;base64,...".to_owned();
                    };
                    let mut guard = output.lock().unwrap();
                    if guard.items.len() >= MAX_OUTPUT_CALLS {
                        return format!(
                            "output limit exceeded: more than {MAX_OUTPUT_CALLS} output calls"
                        );
                    }
                    if guard.chars + data.len() > MAX_OUTPUT_CHARS {
                        return format!(
                            "output limit exceeded: more than {MAX_OUTPUT_CHARS} characters of output"
                        );
                    }
                    guard.push(OutputItem::Image { data, mime_type });
                    String::new()
                }),
            )
            .map_err(rquickjs_error)?;
    }

    // ---- store bridges: writes buffer in `pending_store` (committed on
    // script success); reads see pending writes first. ----
    {
        let pending_store = Arc::clone(&pending_store);
        let committed_store = committed_store.clone();
        let exit_requested = Arc::clone(&exit_requested);
        globals
            .set(
                "__storeValue",
                Func::from(
                    move |key: String, value_json: String, is_delete: bool| -> String {
                        if exit_requested.load(Ordering::Relaxed) {
                            return "script has exited".to_owned();
                        }
                        let mut pending = pending_store.lock().unwrap();
                        if is_delete {
                            // Storing `undefined` deletes the key.
                            pending.insert(key, Value::Null);
                            return String::new();
                        }
                        if value_json.len() > STORE_MAX_VALUE_CHARS {
                            return format!(
                                "store limit exceeded: one value may hold at most {STORE_MAX_VALUE_CHARS} characters of JSON"
                            );
                        }
                        let value: Value = match serde_json::from_str(&value_json) {
                            Ok(value) => value,
                            Err(e) => return format!("store() failed to encode the value: {e}"),
                        };
                        let committed_chars = committed_store
                            .as_ref()
                            .map(|store| {
                                store
                                    .lock()
                                    .unwrap()
                                    .values()
                                    .map(|v| serde_json::to_string(v).map_or(0, |s| s.len()))
                                    .sum::<usize>()
                            })
                            .unwrap_or(0);
                        let pending_chars = pending
                            .values()
                            .map(|v| serde_json::to_string(v).map_or(0, |s| s.len()))
                            .sum::<usize>();
                        if committed_chars + pending_chars > STORE_MAX_TOTAL_CHARS {
                            return format!(
                                "store limit exceeded: all values together may hold at most {STORE_MAX_TOTAL_CHARS} characters of JSON"
                            );
                        }
                        pending.insert(key, value);
                        String::new()
                    },
                ),
            )
            .map_err(rquickjs_error)?;
    }
    {
        let pending_store = Arc::clone(&pending_store);
        let committed_store = committed_store.clone();
        globals
            .set(
                "__loadValue",
                Func::from(move |key: String| -> String {
                    if let Some(value) = pending_store.lock().unwrap().get(&key).cloned() {
                        return serde_json::to_string(&value).unwrap_or_else(|_| "null".to_owned());
                    }
                    if let Some(value) = committed_store
                        .as_ref()
                        .and_then(|store| store.lock().unwrap().get(&key).cloned())
                    {
                        return serde_json::to_string(&value).unwrap_or_else(|_| "null".to_owned());
                    }
                    "null".to_owned()
                }),
            )
            .map_err(rquickjs_error)?;
    }

    // ---- discovery bridges: pure synchronous reads over the tool map, so
    // they are plain closures returning JSON strings. ----
    {
        let tools = Arc::clone(&tools);
        globals
            .set(
                "__searchTools",
                Func::from(move |query: String, opts_json: String| -> String {
                    search_tools_response(&tools, &query, &opts_json)
                }),
            )
            .map_err(rquickjs_error)?;
    }
    {
        let tools = Arc::clone(&tools);
        globals
            .set(
                "__describeTool",
                Func::from(move |name: String| -> String { describe_tool_response(&tools, &name) }),
            )
            .map_err(rquickjs_error)?;
    }
    {
        let tools = Arc::clone(&tools);
        let namespace_infos = Arc::clone(&namespace_infos);
        globals
            .set(
                "__describeNamespace",
                Func::from(move |name: String| -> String {
                    describe_namespace_response(&tools, &namespace_infos, &name)
                }),
            )
            .map_err(rquickjs_error)?;
    }

    {
        let exit_requested = Arc::clone(&exit_requested);
        globals
            .set(
                "__requestExit",
                Func::from(move || exit_requested.store(true, Ordering::Relaxed)),
            )
            .map_err(rquickjs_error)?;
    }

    // ---- JS-side setup: console, text, tools tree, ALL_TOOLS ----
    let all_tools: Vec<Value> = tools
        .values()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description.clone().unwrap_or_default(),
            })
        })
        .collect();
    let all_tools_json = serde_json::to_string(&all_tools)
        .map_err(|e| SandboxError::Engine(format!("serialize tool catalog: {e}")))?;

    let setup = format!(
        r#"
globalThis.__leafPaths = new Set(JSON.parse({tools_spec_literal}));
globalThis.ALL_TOOLS = JSON.parse({all_tools_json});

globalThis.__makeLeaf = (path) => async (args) => {{
    const envelope = JSON.parse(await globalThis.__callTool(path, JSON.stringify(args === undefined ? null : args)));
    if (!envelope.ok) throw new Error(envelope.error);
    return envelope.value;
}};
globalThis.__isNamespace = (prefix) => {{
    for (const path of globalThis.__leafPaths) if (path.startsWith(prefix + ".")) return true;
    return false;
}};
// Props that fall through to undefined unless a real tool owns the name, so
// `tools.toString` is undefined and JSON.stringify(tools) stays {{}} while a
// tool named `constructor` or `__proto__` still resolves (OpenCode's
// "blocked member names" semantics: real paths always win).
globalThis.__reserved = new Set(["then", "toString", "valueOf", "toJSON", "call", "apply", "bind"]);
// The top-level `tools` is an object (pi: JSON.stringify(tools) stays an
// empty object);
// nested namespaces are callable proxies (OpenCode: a namespace without its
// own tool is not callable).
globalThis.__resolvePath = (path) => {{
    if (globalThis.__leafPaths.has(path)) return __makeLeaf(path);
    if (__isNamespace(path)) return __makeNamespace(path);
    return __makeLeaf(path); // unknown leaf: __callTool reports it with suggestions
}};
globalThis.__childSegments = (prefix) => {{
    const out = new Set();
    const at = prefix === "" ? "" : prefix + ".";
    for (const path of globalThis.__leafPaths) {{
        if (!path.startsWith(at)) continue;
        const rest = path.slice(at.length);
        out.add(rest.split(".")[0]);
    }}
    return out;
}};
globalThis.__memberPath = (prefix, prop) => (prefix ? prefix + "." + prop : prop);
globalThis.__objectHandler = (prefix) => ({{
        get(_target, prop) {{
            if (typeof prop !== "string") return undefined;
            if (__reserved.has(prop) && !globalThis.__leafPaths.has(__memberPath(prefix, prop))) return undefined;
            return __resolvePath(__memberPath(prefix, prop));
        }},
        has(_target, prop) {{
            if (typeof prop !== "string") return Reflect.has(_target, prop);
            const path = __memberPath(prefix, prop);
            return globalThis.__leafPaths.has(path) || __isNamespace(path) || Reflect.has(_target, prop);
        }},
        ownKeys(target) {{
            return [...__childSegments(prefix), ...Reflect.ownKeys(target)];
        }},
        getOwnPropertyDescriptor(target, prop) {{
            if (typeof prop === "string" && __childSegments(prefix).has(prop)) {{
                return {{ value: __resolvePath(__memberPath(prefix, prop)), writable: true, enumerable: true, configurable: true }};
            }}
            return Reflect.getOwnPropertyDescriptor(target, prop);
        }},
    }});
globalThis.__makeObjectNamespace = (prefix) => new Proxy({{}}, __objectHandler(prefix));
globalThis.tools = __makeObjectNamespace("");
globalThis.__makeNamespace = function (prefix) {{
    return new Proxy(function () {{}}, {{
        ...__objectHandler(prefix),
        apply(_target, _this, args) {{
            if (globalThis.__leafPaths.has(prefix)) return __makeLeaf(prefix).apply(null, args);
            throw new Error(`Tool '${{prefix}}' is not callable`);
        }},
    }});
}};

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
// image() adds an image output item from a base64 data URI.
globalThis.image = (value) => {{
    const err = globalThis.__emitImage(typeof value === "string" ? value : "");
    if (err) throw new Error(err);
}};
// store(key, value) keeps a JSON value for later codemode calls; storing
// undefined deletes the key. Writes commit only when the script succeeds.
globalThis.store = (key, value) => {{
    const err = globalThis.__storeValue(
        String(key),
        value === undefined ? "null" : JSON.stringify(value),
        value === undefined,
    );
    if (err) throw new Error(err);
}};
globalThis.load = (key) => {{
    const raw = globalThis.__loadValue(String(key));
    return raw === "null" ? undefined : JSON.parse(raw);
}};
// exit() ends the script successfully: the sentinel unwinds the IIFE (the
// rejection handler maps it to a successful envelope); if a try/catch
// swallows it, the exit flag stops every host call afterward and the VM's
// interrupt handler aborts at the next loop or function entry.
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
        tools_spec_literal = serde_json::to_string(
            &serde_json::to_string(&tools.keys().cloned().collect::<Vec<_>>())
                .map_err(|e| SandboxError::Engine(format!("serialize tool spec: {e}")))?
        )
        .map_err(|e| SandboxError::Engine(format!("serialize tool spec literal: {e}")))?,
        all_tools_json = serde_json::to_string(&all_tools_json)
            .map_err(|e| SandboxError::Engine(format!("serialize tool catalog literal: {e}")))?,
    );
    ctx.eval::<(), _>(setup.as_bytes())
        .catch(ctx)
        .map_err(|caught| SandboxError::Script(crate::sandbox::describe_caught(ctx, caught)))?;
    Ok(())
}
/// Resolve a script-supplied tool name to a canonical path: the exact path,
/// then a normalized identifier alias (pi's `tools.my_tool` -> `my-tool`).
fn canonical_tool_path(name: &str, tools: &BTreeMap<String, SandboxTool>) -> Option<String> {
    if tools.contains_key(name) {
        return Some(name.to_owned());
    }
    tools
        .keys()
        .find(|known| normalize_identifier(known) == name)
        .cloned()
}
/// The `searchTools(query, { limit?, namespace?, offset? })` response (pi's
/// shape, with OpenCode's paging): `{ items, remaining, next }` — ranked by
/// BM25 over names and descriptions (pi's default limit 8). An empty query
/// returns tools in path order, so scripts can browse a namespace.
fn search_tools_response(
    tools: &BTreeMap<String, SandboxTool>,
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
    let namespace = options.get("namespace").and_then(Value::as_str);
    let candidates: Vec<(&String, &SandboxTool)> = tools
        .iter()
        .filter(|(path, _)| {
            namespace.is_none_or(|ns| path.starts_with(ns) && path[ns.len()..].starts_with('.'))
        })
        .collect();
    if candidates.is_empty() {
        return json!({ "items": [], "remaining": 0, "next": null }).to_string();
    }
    let index = SearchIndex::build(candidates.iter().map(|(path, tool)| {
        (
            (*path).clone(),
            tool.description.clone().unwrap_or_default(),
            Vec::new(),
        )
    }));
    // Rank everything, then page: `remaining` and `next` need the full count.
    let ranked = index.search(query, candidates.len());
    let paged = ranked.iter().skip(offset).take(limit);
    let items: Vec<Value> = paged
        .map(|hit| json!({ "name": hit.name, "description": hit.description }))
        .collect();
    let remaining = ranked.len().saturating_sub(offset + items.len());
    let next = if remaining > 0 {
        json!(offset + items.len())
    } else {
        json!(null)
    };
    serde_json::to_string(&json!({ "items": items, "remaining": remaining, "next": next }))
        .unwrap_or_else(|_| json!({ "items": [], "remaining": 0, "next": null }).to_string())
}

/// The `describeTool(name)` response: `{ name, description, declaration }` or
/// `null` when the tool is unknown (the JS leaf maps `null` to `undefined`).
fn describe_tool_response(tools: &BTreeMap<String, SandboxTool>, name: &str) -> String {
    let Some(path) = canonical_tool_path(name, tools) else {
        return "null".to_owned();
    };
    let tool = &tools[&path];
    let response = json!({
        "name": path,
        "description": tool.description.clone().unwrap_or_default(),
        "declaration": ts_declaration(
            &path,
            tool.description.as_deref(),
            tool.input_schema.as_ref(),
        ),
    });
    serde_json::to_string(&response).unwrap_or_else(|_| "null".to_owned())
}

/// The `describeNamespace(name)` response: `{ name, description?,
/// instructions?, tools: [...] }` (pi's shape), or `null` when the name
/// resolves to no namespace. The name resolves through the namespace's own
/// name and its aliases (pi: `describeNamespace("docs")` finds
/// `mcp__docs`); members are the tools whose path starts with the
/// canonical namespace plus a `.`.
fn describe_namespace_response(
    tools: &BTreeMap<String, SandboxTool>,
    namespace_infos: &BTreeMap<String, rmcp_codemode::NamespaceInfo>,
    name: &str,
) -> String {
    let Some(canonical) = resolve_namespace_name(namespace_infos, name) else {
        return "null".to_owned();
    };
    let prefix = format!("{canonical}.");
    let members: Vec<&SandboxTool> = tools
        .iter()
        .filter(|(path, _)| path.starts_with(&prefix))
        .map(|(_, tool)| tool)
        .collect();
    if members.is_empty() {
        return "null".to_owned();
    }
    let tools_json: Vec<Value> = members
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description.clone().unwrap_or_default(),
                "declaration": ts_declaration(
                    &tool.name,
                    tool.description.as_deref(),
                    tool.input_schema.as_ref(),
                ),
            })
        })
        .collect();
    let mut response = serde_json::Map::new();
    response.insert("name".to_owned(), json!(canonical));
    let info = namespace_infos.get(&canonical);
    if let Some(description) = info.and_then(|i| i.description.as_deref()) {
        response.insert("description".to_owned(), json!(description));
    }
    if let Some(instructions) = info.and_then(|i| i.instructions.as_deref()) {
        response.insert("instructions".to_owned(), json!(instructions));
    }
    response.insert("tools".to_owned(), json!(tools_json));
    serde_json::to_string(&Value::Object(response)).unwrap_or_else(|_| "null".to_owned())
}

/// Resolve a script-supplied namespace name to a canonical namespace: the
/// name itself, then a namespace whose own name or an alias matches as the
/// same script identifier (pi's `isNamespaceName`), then a path prefix of
/// the tree (a `describeNamespace("api")` for `api.issues.list` still
/// works even without declared metadata).
fn resolve_namespace_name(
    namespace_infos: &BTreeMap<String, rmcp_codemode::NamespaceInfo>,
    name: &str,
) -> Option<String> {
    if namespace_infos.contains_key(name) {
        return Some(name.to_owned());
    }
    let normalized = normalize_identifier(name);
    let by_metadata = namespace_infos
        .iter()
        .find(|(canonical, info)| {
            normalize_identifier(canonical) == normalized
                || info
                    .aliases
                    .iter()
                    .any(|alias| normalize_identifier(alias) == normalized)
        })
        .map(|(canonical, _)| canonical.clone());
    if by_metadata.is_some() {
        return by_metadata;
    }
    // Fall back to a path-prefix namespace of the tree itself.
    Some(name.to_owned())
}
