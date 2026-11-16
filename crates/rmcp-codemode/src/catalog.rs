//! The codemode tool's description catalog and the `mcp_servers` system
//! prompt section — a line-for-line Rust mirror of pi's
//! `extensions/codemode/tool.ts` (`createCodemodeDescription` and helpers)
//! and `extensions/mcp/index.ts` (`renderServersSection`, exposure config).
//! Engine-agnostic string building over [`crate::types::Tool`] (SPEC §7,
//! D9, OQ3).

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
};

use serde_json::{Value, json};

use crate::{
    declarations::{render_tool_output_type, render_tool_sample},
    mcp::{mcp_namespace, mcp_server_summary},
    naming::to_codemode_identifier,
    types::Tool,
};

/// Tools a script may call resolve to their text output when they declare no
/// output schema. (pi: `TEXT_OUTPUT_SCHEMA`, `tool.ts:81`.)
pub const TEXT_OUTPUT_SCHEMA: &str = r#"{"type":"string"}"#;

/// Estimated tokens (characters / 4) the tool sections may use by default.
/// (pi: `DEFAULT_CODEMODE_INLINE_BUDGET`, `settings-defaults.ts:59`.)
pub const DEFAULT_CODEMODE_INLINE_BUDGET: usize = 3000;

/// Characters per token when estimating the cost of a tool section. (pi:
/// `CHARS_PER_TOKEN`, `tool.ts:144`.)
const CHARS_PER_TOKEN: usize = 4;

/// The intro paragraph, verbatim. (pi: `DESCRIPTION_INTRO`,
/// `tool.ts:123-127`.)
const DESCRIPTION_INTRO: &str = "Run JavaScript that calls other tools. The input is raw JavaScript (not JSON, no code fence), run as an async function body in a QuickJS sandbox: top-level `await` and `return` work. No Node, file system, network, or timers.\n- `await tools.<name>({ ...args })` resolves to a string, or an object if the tool's declaration says so, and rejects with an Error on failure. A tool whose declaration returns an object resolves on failure too when it returned that object; check its fields. Calls still running when the script ends are cancelled.\n- Optional first line: `// @options: {\"max_output_tokens\": 10000, \"timeout_ms\": 60000}`";

/// One line per global. The `models` namespace is pi's classifier surface,
/// which our crate does not provide — hosts pass `false`. (pi:
/// `describeGlobals`, `tool.ts:128-139`.)
pub fn describe_globals(models: bool) -> String {
    let mut lines = vec![
        "Globals:".to_string(),
        "- `text(value)`, `image(dataUrlOrImageBlock)`, `console.log(...)`, and top-level `return` add output; `exit()` ends the script. With several text items, each starts with a `==> text N/M <==` line, and `console` lines follow the other output in one `<console_output>` block. `image()` also saves the image to a temp file and the result names its path.".to_string(),
        "- `store(key, value)` and `load(key)` keep JSON values across codemode calls.".to_string(),
        "- `ALL_TOOLS`, `await searchTools(query, { limit?, namespace? })`, `await describeTool(name)`, `await describeNamespace(name)`: find unlisted tools, such as MCP tools.".to_string(),
    ];
    if models {
        lines.push("- `models`: classifiers and image generation.".to_string());
    }
    lines.join("\n")
}

/// A tool namespace for grouping in the description. (pi: `ToolNamespace`.)
#[derive(Debug, Clone, PartialEq)]
pub struct ToolNamespace {
    pub name: String,
    pub description: Option<String>,
}

/// What a script sees of a tool: name, description (with prompt guidelines
/// appended), and schemas. (pi: `toCodemodeDeclaration`, `tool.ts:150-161`.)
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogDeclaration {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Option<Value>,
    pub output_schema: Option<Value>,
}

/// pi `toCodemodeDeclaration`: guidelines render as bullets after the
/// description; tools without an output schema resolve to their text output
/// (`{"type":"string"}`).
pub fn to_codemode_declaration(tool: &Tool, guidelines: &[String]) -> CatalogDeclaration {
    let bullets = guidelines
        .iter()
        .map(|guideline| guideline.trim())
        .filter(|guideline| !guideline.is_empty())
        .map(|guideline| format!("- {guideline}"))
        .collect::<Vec<_>>();
    let description = if bullets.is_empty() {
        tool.description.clone()
    } else {
        Some(match tool.description.as_deref().map(str::trim) {
            Some(trimmed) if !trimmed.is_empty() => {
                format!("{trimmed}\n\n{}", bullets.join("\n"))
            }
            _ => bullets.join("\n"),
        })
    };
    CatalogDeclaration {
        name: tool.name.clone(),
        description,
        input_schema: tool.input_schema.clone(),
        output_schema: Some(
            tool.output_schema
                .clone()
                .unwrap_or_else(|| json!({ "type": "string" })),
        ),
    }
}

/// `### \`id\` (\`raw name\`)` followed by the tool's sample. (pi:
/// `renderToolSection`, `tool.ts:185-189`.)
pub fn render_tool_section(declaration: &CatalogDeclaration) -> String {
    let id = to_codemode_identifier(&declaration.name);
    let heading = if id == declaration.name {
        format!("### `{id}`")
    } else {
        format!("### `{id}` (`{}`)", declaration.name)
    };
    let tool = Tool {
        name: declaration.name.clone(),
        description: declaration.description.clone(),
        input_schema: declaration.input_schema.clone(),
        output_schema: declaration.output_schema.clone(),
        spread: false,
        signature: None,
        execute: Arc::new(|_args, _context| {
            Box::pin(async {
                Err(crate::types::ToolError {
                    message: "catalog only".into(),
                })
            })
        }),
    };
    format!("{heading}\n{}", render_tool_sample(&tool, None).trim())
}

#[derive(Clone)]
struct CatalogEntry {
    name: String,
    section: String,
    cost: usize,
}

struct CatalogGroup {
    namespace: Option<ToolNamespace>,
    entries: Vec<CatalogEntry>,
}

/// Pick the tool sections that fit the budget, like OpenCode's catalog: in
/// each round every group (tools without a namespace first, then namespaces
/// by name) places its cheapest remaining tool; a group whose next tool does
/// not fit drops out while the others continue. Every namespace is
/// represented before any namespace is complete. (pi: `selectCatalog`,
/// `tool.ts:208-223`.)
fn select_catalog(groups: &[CatalogGroup], budget: Option<usize>) -> BTreeSet<String> {
    let Some(budget) = budget else {
        return groups
            .iter()
            .flat_map(|group| group.entries.iter().map(|entry| entry.name.clone()))
            .collect();
    };
    let queues: Vec<Vec<CatalogEntry>> = groups
        .iter()
        .map(|group| {
            let mut entries = group.entries.clone();
            entries.sort_by_key(|entry| entry.cost);
            entries
        })
        .collect();
    let mut shown = BTreeSet::new();
    let mut remaining = budget;
    let mut active: Vec<Vec<CatalogEntry>> = queues
        .into_iter()
        .filter(|queue| !queue.is_empty())
        .collect();
    while !active.is_empty() {
        for queue in active.iter_mut() {
            let Some(next) = queue.first() else {
                continue;
            };
            if next.cost > remaining {
                continue;
            }
            remaining -= next.cost;
            shown.insert(next.name.clone());
            queue.remove(0);
        }
        // A group whose next tool does not fit drops out while the others
        // continue (pi's filter shape).
        active.retain(|queue| !queue.is_empty() && queue[0].cost <= remaining);
    }
    shown
}

/// Options of [`create_codemode_description`]. (pi:
/// `CodemodeDescriptionOptions`.)
pub struct DescriptionOptions<'a> {
    /// Namespace of each tool, by tool name. Tools of one namespace are
    /// listed under one heading.
    pub namespaces: &'a HashMap<String, ToolNamespace>,
    /// Prompt guidelines of each tool, by tool name, listed after its
    /// description.
    pub guidelines: &'a HashMap<String, Vec<String>>,
    /// Tools that are callable but never listed with their declaration
    /// (`deferred` exposure).
    pub deferred: &'a HashSet<String>,
    /// Estimated tokens (characters / 4) the tool sections may use. `None`
    /// lists every tool that is not deferred.
    pub inline_budget: Option<usize>,
}

/// The model-facing description: the intro, the globals list, and one
/// section per listed tool grouped by namespace. Deferred tools are never
/// listed and do not affect the description at all, so it stays the same
/// while tools are registered late. (pi: `createCodemodeDescription`,
/// `tool.ts:228-273`.)
pub fn create_codemode_description(tools: &[Tool], options: &DescriptionOptions<'_>) -> String {
    let declarations = tools
        .iter()
        .filter(|tool| !options.deferred.contains(&tool.name))
        .map(|tool| {
            let empty: Vec<String> = Vec::new();
            to_codemode_declaration(tool, options.guidelines.get(&tool.name).unwrap_or(&empty))
        })
        .collect::<Vec<_>>();
    // pi groups by namespace name key; the ungrouped group sorts first, then
    // namespaces by name (pi localeCompare ≈ scalar order for these names).
    let mut groups: Vec<CatalogGroup> = vec![CatalogGroup {
        namespace: None,
        entries: Vec::new(),
    }];
    for declaration in &declarations {
        let namespace = options.namespaces.get(&declaration.name).cloned();
        let section = render_tool_section(declaration);
        let entry = CatalogEntry {
            name: declaration.name.clone(),
            cost: section.len().div_ceil(CHARS_PER_TOKEN),
            section,
        };
        match groups.iter_mut().find(|group| {
            group.namespace.as_ref().map(|ns| Some(&ns.name))
                == namespace.as_ref().map(|ns| Some(&ns.name))
        }) {
            Some(group) => group.entries.push(entry),
            None => groups.push(CatalogGroup {
                namespace,
                entries: vec![entry],
            }),
        }
    }
    groups.sort_by(|a, b| match (&a.namespace, &b.namespace) {
        (None, _) => std::cmp::Ordering::Less,
        (_, None) => std::cmp::Ordering::Greater,
        (Some(a), Some(b)) => a.name.cmp(&b.name),
    });
    let shown = select_catalog(&groups, options.inline_budget);

    let mut sections = vec![DESCRIPTION_INTRO.to_string(), describe_globals(false)];
    if declarations.is_empty() {
        return sections.join("\n\n");
    }

    let mut tool_sections = vec!["Nested tools:".to_string()];
    for group in &groups {
        let visible = group
            .entries
            .iter()
            .filter(|entry| shown.contains(&entry.name))
            .cloned()
            .collect::<Vec<_>>();
        if let Some(namespace) = &group.namespace {
            // Only tools that did not fit the budget are counted as not listed here.
            let listing = if visible.len() == group.entries.len() {
                ""
            } else if visible.is_empty() {
                " (tools not listed)"
            } else {
                " (some tools not listed)"
            };
            let description = namespace.description.as_deref().map(str::trim);
            tool_sections.push(match description {
                Some(description) if !description.is_empty() => {
                    format!("## {}{listing}\n{description}", namespace.name)
                }
                _ => format!("## {}{listing}", namespace.name),
            });
        }
        for entry in &visible {
            tool_sections.push(entry.section.clone());
        }
    }
    sections.push(tool_sections.join("\n\n"));
    sections.join("\n\n")
}

/// What a script call resolves to, in one line: `a string`, the field names
/// of an object, the field names of each object in a union, the rendered
/// type when it fits on one line, or a pointer to `describeTool()` for
/// anything longer. (pi: `describeOutput`, `tool.ts:308-316`.)
pub fn describe_output(name: &str, schema: Option<&Value>) -> String {
    let kind = render_tool_output_type(schema);
    if kind == "string" {
        return "a string".into();
    }
    let fields = schema.and_then(|schema| {
        describe_fields(schema, schema, 1).or_else(|| describe_union_fields(schema, schema))
    });
    if let Some(fields) = fields {
        return format!("`{fields}`");
    }
    if !kind.contains('\n') {
        return format!("`{kind}`");
    }
    format!(
        "a value; `await describeTool({})` shows its type",
        json!(name)
    )
}

/// The field names of an object schema one level deep (const values inline).
/// (pi: `describeFields`, `tool.ts:283-300`.)
fn describe_fields(schema: &Value, root: &Value, depth: usize) -> Option<String> {
    let resolved = follow_ref(schema, root);
    let object = resolved.as_object()?;
    if object.get("type")?.as_str()? != "object" {
        return None;
    }
    let properties = object.get("properties")?.as_object()?;
    let required: HashSet<&str> = object
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let fields = properties
        .iter()
        .map(|(name, field)| {
            let const_value = follow_ref(field, root).get("const");
            let nested = match const_value {
                Some(const_value) if !const_value.is_null() => Some(const_value.to_string()),
                _ if depth > 0 => describe_fields(field, root, depth - 1),
                _ => None,
            };
            format!(
                "{name}{}{}",
                if required.contains(name.as_str()) {
                    ""
                } else {
                    "?"
                },
                nested
                    .map(|nested| format!(": {nested}"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("{{ {fields} }}"))
}

/// The field names of each variant of an `anyOf`/`oneOf` whose variants are
/// all objects, joined by `|`. (pi: `describeUnionFields`,
/// `tool.ts:318-332`.)
fn describe_union_fields(schema: &Value, root: &Value) -> Option<String> {
    let resolved = follow_ref(schema, root);
    let variants = resolved
        .get("anyOf")
        .or_else(|| resolved.get("oneOf"))
        .and_then(Value::as_array)?;
    if variants.is_empty() {
        return None;
    }
    let described: Option<Vec<String>> = variants
        .iter()
        .map(|variant| describe_fields(variant, root, 0))
        .collect();
    described.map(|described| described.join(" | "))
}

/// The subschema the local `$ref` chain of `schema` names in `root`; cyclic
/// and remote references stop. (pi: `followRef`, `tool.ts:334-348`.)
fn follow_ref<'a>(schema: &'a Value, root: &'a Value) -> &'a Value {
    let mut seen: HashSet<String> = HashSet::new();
    let mut current = schema;
    loop {
        let Some(reference) = current.get("$ref").and_then(Value::as_str) else {
            return current;
        };
        if !seen.insert(reference.to_string()) {
            return current;
        }
        let Some(resolved) = crate::declarations::resolve_schema_ref(reference, root) else {
            return current;
        };
        current = resolved;
    }
}

/// What a tool's description says about calling it from scripts, in one line.
/// (pi: the `describeScriptCall` template, `tool.ts:355`.)
pub fn describe_script_call(tool: &Tool) -> String {
    let declaration = to_codemode_declaration(tool, &[]);
    format!(
        "{}\n\nCodemode: `tools.{}(args)` resolves to {}.",
        tool.description.as_deref().unwrap_or("").trim(),
        to_codemode_identifier(&tool.name),
        describe_output(&tool.name, declaration.output_schema.as_ref()),
    )
}

/// The codemode tool's presentation mode. (pi: `Mode` / `SETTINGS_DEFAULTS
/// .codemode.mode` — `on` is the default.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodemodeMode {
    /// Descriptions of declared callable tools say how scripts call them; the
    /// description lists only callable tools without `direct` exposure.
    On,
    /// The description lists every callable tool, and requests leave out the
    /// declarations of active `direct` tools.
    Only,
}

/// Which callable tools the description lists. (pi: `prepareCodemodeLoadout`,
/// `tool.ts:360-366`.)
pub fn listed_tools<'a>(
    mode: CodemodeMode,
    callable: impl IntoIterator<Item = &'a Tool>,
    is_direct: impl Fn(&Tool) -> bool,
) -> Vec<&'a Tool> {
    let callable: Vec<&Tool> = callable.into_iter().collect();
    match mode {
        CodemodeMode::Only => callable,
        CodemodeMode::On => callable
            .into_iter()
            .filter(|tool| !is_direct(tool))
            .collect(),
    }
}

// ------------------------------------------------- mcp_servers section (OQ3)

/// Name of the system prompt section that lists servers whose tools are not
/// declared. (pi: `MCP_SERVERS_SECTION`, `index.ts:157`.)
pub const MCP_SERVERS_SECTION: &str = "mcp_servers";

/// Characters of a server description in the section. (pi:
/// `MAX_SERVER_DESCRIPTION_CHARS`, `index.ts:159`.)
pub const MAX_SERVER_DESCRIPTION_CHARS: usize = 250;

/// Characters of the whole section. Descriptions shrink to fit; when the
/// server lines alone do not fit, the last servers are left out and counted
/// in a closing line. (pi: `MAX_SERVERS_SECTION_CHARS`, `index.ts:164`.)
pub const MAX_SERVERS_SECTION_CHARS: usize = 4096;

/// How a server's tools are reached, as the section's intro spells out. (pi:
/// the `reaches` set — our surfaces are `codemode` and `tool_search`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerReach {
    Codemode,
    ToolSearch,
}

/// What the `mcp_servers` section needs of a server. (pi:
/// `McpServerListing`.)
pub struct McpServerListing<'a> {
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub instructions: Option<&'a str>,
    pub reach: ServerReach,
    pub enabled: bool,
}

/// The section's first line, explaining the ways of reaching tools that the
/// listed servers use. (pi: `serversSectionIntro`, `index.ts:168-172`.)
fn servers_section_intro(reaches: &HashSet<&str>) -> String {
    let mut intro = "MCP servers whose tools are not declared to you.".to_string();
    if reaches.contains("codemode") {
        intro += " Call the tools of `codemode` servers from codemode scripts.";
    }
    if reaches.contains("tool_search") {
        intro += " Load the tools of `tool_search` servers with `tool_search`.";
    }
    intro
}

/// `sliceHead` without the UTF-16 surrogate dance — Rust strings are UTF-8,
/// so a character-boundary-safe cut is the faithful adaptation. (pi:
/// `sliceHead`, `truncate.ts:279-285`.)
fn slice_head(text: &str, max_units: usize) -> &str {
    let end = max_units.min(text.len());
    let mut end = end;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn truncate_text(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max <= 1 {
        return String::new();
    }
    format!("{}\u{2026}", slice_head(text, max - 1).trim_end())
}

/// The `mcp_servers` section: every enabled server with codemode or
/// deferred (tool_search) tools, with how its tools are reached and a
/// one-line summary; servers sorted by name; descriptions shrink to fit;
/// overflow servers collapse into a closing line. `None` when there are no
/// such servers. (pi: `renderServersSection`, `index.ts:190-218`.)
pub fn render_servers_section(servers: &[McpServerListing<'_>]) -> Option<String> {
    let mut listed: Vec<&McpServerListing<'_>> =
        servers.iter().filter(|server| server.enabled).collect();
    listed.sort_by(|a, b| a.name.cmp(b.name));
    let listed: Vec<&McpServerListing<'_>> = listed
        .into_iter()
        .filter(|server| match server.reach {
            ServerReach::Codemode | ServerReach::ToolSearch => true,
        })
        .collect();
    if listed.is_empty() {
        return None;
    }
    let reaches: HashSet<&str> = listed
        .iter()
        .map(|server| match server.reach {
            ServerReach::Codemode => "codemode",
            ServerReach::ToolSearch => "tool_search",
        })
        .collect();
    let intro = servers_section_intro(&reaches);
    let heads: Vec<String> = listed
        .iter()
        .map(|server| {
            let reach = match server.reach {
                ServerReach::Codemode => "codemode",
                ServerReach::ToolSearch => "tool_search",
            };
            format!("- {} ({reach})", mcp_namespace(server.name))
        })
        .collect();
    let omitted = |count: usize| -> Vec<String> {
        if count > 0 {
            vec![format!(
                "- … {count} more server{}; find their tools with searchTools()",
                if count == 1 { "" } else { "s" }
            )]
        } else {
            Vec::new()
        }
    };
    // Characters of the intro, the first `kept` server lines without
    // descriptions, and the omission line.
    let size = |kept: usize| -> usize {
        [intro.as_str()]
            .into_iter()
            .chain(heads.iter().take(kept).map(String::as_str))
            .chain(omitted(listed.len() - kept).iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .count()
    };
    let mut kept = listed.len();
    while kept > 0 && size(kept) > MAX_SERVERS_SECTION_CHARS {
        kept -= 1;
    }
    // Each description also takes a ": " separator. Negative room (server
    // lines alone overflow) means no descriptions at all — pi's
    // `perServer > 0` check; Rust needs the saturating form.
    let per_server = (if kept == 0 {
        0
    } else {
        (MAX_SERVERS_SECTION_CHARS - size(kept))
            .checked_div(kept)
            .unwrap_or(0)
    })
    .saturating_sub(2)
    .min(MAX_SERVER_DESCRIPTION_CHARS);
    let lines = listed
        .iter()
        .enumerate()
        .take(kept)
        .map(|(index, server)| {
            let summary = if per_server > 0 {
                truncate_text(
                    &mcp_server_summary(server.description, server.instructions),
                    per_server,
                )
            } else {
                String::new()
            };
            if summary.is_empty() {
                heads[index].clone()
            } else {
                format!("{}: {summary}", heads[index])
            }
        })
        .collect::<Vec<_>>();
    Some(
        [intro]
            .into_iter()
            .chain(lines)
            .chain(omitted(listed.len() - kept))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

// ----------------------------------------------------- exposure config (OQ3)

/// Exposure of a server's tools. (pi: `McpExposure` — direct / codemode /
/// deferred / hidden.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExposure {
    /// Declared natively to the model; excluded from the catalog in `on` mode.
    Direct,
    /// Callable from codemode scripts, found with `searchTools` (default).
    Codemode,
    /// Loaded by the native `tool_search` tool, then declared and called
    /// directly.
    Deferred,
    /// Not reachable at all.
    Hidden,
}

impl ToolExposure {
    /// The config spelling. (pi: the four `MCP_EXPOSURES` values.)
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Codemode => "codemode",
            Self::Deferred => "deferred",
            Self::Hidden => "hidden",
        }
    }

    /// Parse a config spelling; older aliases were resolved by pi's config
    /// load before this sees them.
    pub fn parse(spelling: &str) -> Option<Self> {
        match spelling {
            "direct" => Some(Self::Direct),
            "codemode" => Some(Self::Codemode),
            "deferred" => Some(Self::Deferred),
            "hidden" => Some(Self::Hidden),
            _ => None,
        }
    }
}

/// Exposure of one server's tool: the tool's `toolExposure` override (exact
/// name first, then the first matching `*` pattern), else the server's
/// `exposure`, else `codemode`. (pi: `getMcpToolExposure`,
/// `mcp-servers.ts:170-178`.)
pub fn get_tool_exposure(
    server_exposure: Option<ToolExposure>,
    overrides: &[(String, ToolExposure)],
    tool_name: &str,
) -> ToolExposure {
    for (name, exposure) in overrides {
        if name == tool_name {
            return *exposure;
        }
    }
    for (pattern, exposure) in overrides {
        if pattern.contains('*') && tool_pattern_matches(pattern, tool_name) {
            return *exposure;
        }
    }
    server_exposure.unwrap_or(ToolExposure::Codemode)
}

/// A `*` pattern as an anchored wildcard: every part between `*`s must
/// appear in order (pi: `toolPatternRegExp` splits on `*`, escapes the
/// parts, and joins with `.*`, anchored).
pub fn tool_pattern_matches(pattern: &str, tool_name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return tool_name == pattern;
    }
    let first = parts[0];
    let last = parts[parts.len() - 1];
    let mut rest = tool_name;
    if !first.is_empty() {
        if !rest.starts_with(first) {
            return false;
        }
        rest = &rest[first.len()..];
    }
    if !last.is_empty() {
        if !rest.ends_with(last) {
            return false;
        }
        rest = &rest[..rest.len() - last.len()];
    }
    for part in &parts[1..parts.len() - 1] {
        if part.is_empty() {
            continue;
        }
        match rest.find(part) {
            Some(found) => rest = &rest[found + part.len()..],
            None => return false,
        }
    }
    true
}
