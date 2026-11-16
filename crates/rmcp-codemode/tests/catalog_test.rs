//! §7 oracle: the codemode description catalog and the `mcp_servers` system
//! prompt section — pi sources labeled per case (`tool.ts` /
//! `extensions/mcp/index.ts`).

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use rmcp_codemode::{
    catalog::{
        CodemodeMode, DEFAULT_CODEMODE_INLINE_BUDGET, DescriptionOptions,
        MAX_SERVERS_SECTION_CHARS, MCP_SERVERS_SECTION, McpServerListing, ServerReach,
        ToolExposure, ToolNamespace, create_codemode_description, describe_output,
        describe_script_call, get_tool_exposure, listed_tools, render_servers_section,
        tool_pattern_matches,
    },
    types::{Tool, ToolContext, ToolFn},
};
use serde_json::{Value, json};

fn catalog_tool(name: &str, description: &str, output_schema: Option<Value>) -> Tool {
    Tool {
        name: name.into(),
        description: Some(description.into()),
        input_schema: Some(json!({ "type": "object", "properties": {} })),
        output_schema,
        spread: false,
        signature: None,
        execute: {
            let execute: ToolFn = Arc::new(|_args: Value, _context: ToolContext| {
                Box::pin(async {
                    Err(rmcp_codemode::types::ToolError {
                        message: "catalog only".into(),
                    })
                })
            });
            execute
        },
    }
}

type CatalogEmpty = (
    HashMap<String, ToolNamespace>,
    HashMap<String, Vec<String>>,
    HashSet<String>,
);

fn no_options() -> DescriptionOptions<'static> {
    let empty: &'static CatalogEmpty = Box::leak(Box::default());
    DescriptionOptions {
        namespaces: &empty.0,
        guidelines: &empty.1,
        deferred: &empty.2,
        inline_budget: None,
    }
}

/// pi `tool.ts:228-273`: intro, globals, `Nested tools:`, ungrouped tools
/// first, then namespaces by name; headings `### \`id\`` with the raw name in
/// parentheses when it needed normalizing.
#[test]
fn description_structure_groups_ungrouped_first_then_namespaces_by_name() {
    let tools = vec![
        catalog_tool("zeta", "Ungrouped tool.", None),
        catalog_tool("web.search", "Namespace tool.", None),
        catalog_tool("alpha", "Another ungrouped.", None),
    ];
    let mut namespaces = HashMap::new();
    namespaces.insert(
        "web.search".to_string(),
        ToolNamespace {
            name: "web".into(),
            description: Some("Web tools.".into()),
        },
    );
    let options = DescriptionOptions {
        namespaces: &namespaces,
        ..no_options()
    };
    let text = create_codemode_description(&tools, &options);
    let heading = text.find("## web").unwrap();
    let zeta = text.find("### `zeta`").unwrap();
    let alpha = text.find("### `alpha`").unwrap();
    let web_search = text.find("### `web_search` (`web.search`)").unwrap();
    assert!(text.starts_with("Run JavaScript that calls other tools."));
    assert!(text.contains("Globals:\n- `text(value)`"));
    assert!(text.contains("Nested tools:"));
    // Within the ungrouped group, tools render in registration order (pi
    // pushes entries in tool order and renders them unsorted); both precede
    // the namespace heading.
    assert!(zeta < alpha);
    assert!(zeta < heading);
    assert!(alpha < heading, "{text}");
    assert!(heading < web_search, "{text}");
    // Namespace heading carries its description.
    assert!(text.contains("## web\nWeb tools."), "{text}");
}

/// pi `tool.ts:229-231`: deferred tools are never listed and do not affect
/// the description at all — the text is identical whether or not they are in
/// the callable list, while the `deferred` set covers them.
#[test]
fn deferred_tools_do_not_affect_the_description() {
    let listed = vec![catalog_tool("read", "Read a file.", None)];
    let with_deferred = vec![
        catalog_tool("read", "Read a file.", None),
        catalog_tool("web.fetch", "Fetch a page.", None),
    ];
    let mut deferred = HashSet::new();
    deferred.insert("web.fetch".to_string());
    let options = DescriptionOptions {
        deferred: &deferred,
        ..no_options()
    };
    let without = create_codemode_description(&listed, &options);
    let with = create_codemode_description(&with_deferred, &options);
    assert_eq!(without, with);
    assert!(!with.contains("web_fetch"));
    assert!(with.contains("### `read`"));
}

/// pi `tool.ts:208-223`: round-robin packing — each round every group places
/// its cheapest remaining tool; a group that cannot fit drops out while the
/// others continue; every namespace is represented before any is complete.
#[test]
fn inline_budget_round_robin_represents_every_namespace() {
    // Namespace `big` has one expensive tool; `small` has two cheap ones.
    // Budget 250 tokens: round 1 places big.scan (~190) and small.a (~57);
    // round 2's small.b no longer fits — `big` is represented before `small`
    // is complete, and the partial group says so.
    let tools = vec![
        catalog_tool("big.scan", &"x".repeat(600), None),
        catalog_tool("small.a", &"a".repeat(80), None),
        catalog_tool("small.b", &"b".repeat(80), None),
    ];
    let mut namespaces = HashMap::new();
    namespaces.insert(
        "big.scan".to_string(),
        ToolNamespace {
            name: "big".into(),
            description: None,
        },
    );
    namespaces.insert(
        "small.a".to_string(),
        ToolNamespace {
            name: "small".into(),
            description: None,
        },
    );
    namespaces.insert(
        "small.b".to_string(),
        ToolNamespace {
            name: "small".into(),
            description: None,
        },
    );
    // Costs (chars/4 ≈ 190 for big, ≈57 for small): budget 250 fits big.scan
    // plus small.a in round 1; small.b drops in round 2.
    let options = DescriptionOptions {
        namespaces: &namespaces,
        inline_budget: Some(250),
        ..no_options()
    };
    let text = create_codemode_description(&tools, &options);
    assert!(text.contains("## big"), "{text}");
    assert!(text.contains("## small (some tools not listed)"), "{text}");
    assert!(text.contains("### `small_a`"), "{text}");
    assert!(!text.contains("### `small_b`"), "{text}");
    assert!(text.contains("### `big_scan`"), "{text}");
    assert_eq!(DEFAULT_CODEMODE_INLINE_BUDGET, 3000);
}

/// pi `tool.ts:210`: no budget lists everything that is not deferred.
#[test]
fn no_budget_lists_everything() {
    let tools = vec![
        catalog_tool("a", "A.", None),
        catalog_tool("b", "B.", None),
        catalog_tool("c", "C.", None),
    ];
    let text = create_codemode_description(&tools, &no_options());
    for name in ["### `a`", "### `b`", "### `c`"] {
        assert!(text.contains(name), "{name} missing");
    }
}

/// pi `tool.ts:150-161` + `renderToolSection`: guidelines render as bullets
/// after the description.
#[test]
fn guidelines_render_as_bullets_in_the_section() {
    let tools = vec![catalog_tool("read", "Read a file.\nSecond line.", None)];
    let mut guidelines = HashMap::new();
    guidelines.insert(
        "read".to_string(),
        vec![
            "Pass an absolute path.".to_string(),
            "  ".to_string(),
            "Handles UTF-8.".to_string(),
        ],
    );
    let options = DescriptionOptions {
        guidelines: &guidelines,
        ..no_options()
    };
    let text = create_codemode_description(&tools, &options);
    assert!(
        text.contains("Read a file.\nSecond line.\n\n- Pass an absolute path.\n- Handles UTF-8."),
        "{text}"
    );
}

/// pi `describeOutput`/`describeFields`/`describeScriptCall`: one-line
/// resolution summaries.
#[test]
fn script_call_summaries_describe_the_output() {
    let string_tool = catalog_tool("read", "Read.", None);
    assert!(
        describe_script_call(&string_tool)
            .contains("Codemode: `tools.read(args)` resolves to a string.")
    );
    let object_tool = catalog_tool(
        "search",
        "Search.",
        Some(json!({
            "type": "object",
            "properties": {
                "results": { "type": "array", "items": { "type": "object" } },
                "query": { "type": "string" },
            },
            "required": ["results"],
        })),
    );
    assert!(
        describe_script_call(&object_tool)
            .contains("Codemode: `tools.search(args)` resolves to `{ results, query? }`."),
        "{}",
        describe_script_call(&object_tool)
    );
    let const_tool = catalog_tool(
        "action",
        "Act.",
        Some(json!({
            "type": "object",
            "properties": { "action": { "const": "start" }, "id": { "type": "string" } },
            "required": ["action", "id"],
        })),
    );
    assert!(
        describe_script_call(&const_tool).contains("resolves to `{ action: \"start\", id }`."),
        "{}",
        describe_script_call(&const_tool)
    );
    let union_tool = catalog_tool(
        "either",
        "Either.",
        Some(json!({
            "anyOf": [
                { "type": "object", "properties": { "a": { "type": "string" } }, "required": ["a"] },
                { "type": "object", "properties": { "b": { "type": "string" } }, "required": ["b"] },
            ],
        })),
    );
    assert!(
        describe_script_call(&union_tool).contains("resolves to `{ a } | { b }`."),
        "{}",
        describe_script_call(&union_tool)
    );
    // A large multiline type points at describeTool instead (#11501).
    let deep_tool = catalog_tool(
        "deep",
        "Deep.",
        Some(json!({ "anyOf": [{ "type": "object" }, { "type": "string" }] })),
    );
    let summary = describe_script_call(&deep_tool);
    assert!(
        summary.contains("describeTool") || summary.contains("resolves to"),
        "{summary}"
    );
    assert!(
        describe_output("x", Some(&json!("not a schema"))).starts_with("`"),
        "{summary}"
    );
}

/// pi `prepareCodemodeLoadout` (`tool.ts:360-366`): `on` mode lists callable
/// tools without `direct` exposure; `only` mode lists every callable tool.
#[test]
fn mode_on_excludes_direct_from_the_listing() {
    let tools = vec![
        catalog_tool("direct-tool", "Declared natively.", None),
        catalog_tool("script-tool", "Script-only.", None),
    ];
    let is_direct = |tool: &Tool| tool.name == "direct-tool";
    let on = listed_tools(CodemodeMode::On, &tools, is_direct);
    let only = listed_tools(CodemodeMode::Only, &tools, is_direct);
    assert_eq!(
        on.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(),
        ["script-tool"]
    );
    assert_eq!(only.len(), 2);
}

// ---- pi extensions/mcp/index.ts > renderServersSection ---------------------

/// pi `renderServersSection` (`index.ts:190-218`): servers sorted by name,
/// reach lines, summaries ≤250 chars, section ≤4096, overflow collapsed.
#[test]
fn servers_section_sorts_and_renders_summaries() {
    let servers = vec![
        McpServerListing {
            name: "web",
            description: Some("Web tools.\nMore detail."),
            instructions: None,
            reach: ServerReach::Codemode,
            enabled: true,
        },
        McpServerListing {
            name: "dev-radius",
            description: None,
            instructions: Some("Radius operations.\nExtra"),
            reach: ServerReach::ToolSearch,
            enabled: true,
        },
        McpServerListing {
            name: "disabled",
            description: Some("Never shown."),
            instructions: None,
            reach: ServerReach::Codemode,
            enabled: false,
        },
    ];
    let section = render_servers_section(&servers).unwrap();
    let lines: Vec<&str> = section.split('\n').collect();
    assert_eq!(
        lines[0],
        "MCP servers whose tools are not declared to you. Call the tools of `codemode` servers from codemode scripts. Load the tools of `tool_search` servers with `tool_search`."
    );
    assert_eq!(
        lines[1],
        "- mcp__dev_radius (tool_search): Radius operations."
    );
    assert_eq!(lines[2], "- mcp__web (codemode): Web tools.");
    assert!(section.chars().count() <= MAX_SERVERS_SECTION_CHARS);
    assert_eq!(MCP_SERVERS_SECTION, "mcp_servers");
}

/// Long summaries shrink to fit the section budget; when the server lines
/// alone do not fit, the last servers collapse into the omission line.
#[test]
fn servers_section_shrinks_descriptions_then_drops_servers() {
    // 60 servers: heads fit, so descriptions shrink but everyone stays.
    let many: Vec<McpServerListing<'_>> = (0..60)
        .map(|index| {
            let description =
                format!("Server {index} with a fairly long description of its purpose.");
            let name: String = format!("server-{index:03}");
            let description: &'static str = Box::leak(description.into_boxed_str());
            let name: &'static str = Box::leak(name.into_boxed_str());
            McpServerListing {
                name,
                description: Some(description),
                instructions: None,
                reach: ServerReach::Codemode,
                enabled: true,
            }
        })
        .collect();
    let section = render_servers_section(&many).unwrap();
    assert!(section.chars().count() <= MAX_SERVERS_SECTION_CHARS);
    assert!(!section.contains("more servers"), "{section}");
    // Descriptions shrink but stay within their cap.
    assert!(
        section.contains(": Server 0 with a fairly long descri…"),
        "{section}"
    );
}

/// 80 servers: the bare head lines alone overflow 4096 characters, so
/// descriptions get zero room (pi's `perServer > 0` being false) and the
/// last servers collapse into the omission line.
#[test]
fn servers_section_overflow_drops_servers_without_descriptions() {
    let many: Vec<McpServerListing<'_>> = (0..150)
        .map(|index| {
            let description =
                format!("Server {index} with a fairly long description of its purpose.");
            let name: String = format!("server-{index:03}");
            let description: &'static str = Box::leak(description.into_boxed_str());
            let name: &'static str = Box::leak(name.into_boxed_str());
            McpServerListing {
                name,
                description: Some(description),
                instructions: None,
                reach: ServerReach::Codemode,
                enabled: true,
            }
        })
        .collect();
    let section = render_servers_section(&many).unwrap();
    assert!(
        section.chars().count() <= MAX_SERVERS_SECTION_CHARS,
        "{}",
        section.chars().count()
    );
    assert!(
        section.contains("more servers; find their tools with searchTools()"),
        "{section}"
    );
    // The overflow servers' heads are bare: no descriptions fit.
    assert!(!section.contains(": Server"), "{section}");
    assert!(
        section.contains("- mcp__server_000 (codemode)"),
        "{section}"
    );
}

/// A long description is cut at 250 characters with an ellipsis (pi
/// `MAX_SERVER_DESCRIPTION_CHARS` + `truncate`).
#[test]
fn server_summary_is_capped_at_250_characters() {
    let long: &'static str = Box::leak("y".repeat(400).into_boxed_str());
    let servers = vec![McpServerListing {
        name: "big",
        description: Some(long),
        instructions: None,
        reach: ServerReach::Codemode,
        enabled: true,
    }];
    let section = render_servers_section(&servers).unwrap();
    let summary_line = section.split('\n').nth(1).unwrap();
    // `- mcp__big (codemode): ` is 23 chars; the summary is capped at 250.
    let summary = summary_line
        .strip_prefix("- mcp__big (codemode): ")
        .unwrap();
    assert!(
        summary.chars().count() <= 250,
        "{}",
        summary.chars().count()
    );
    assert!(
        summary.ends_with('…') || summary.chars().count() == 250,
        "{summary}"
    );
}

/// No enabled indirect servers → no section at all (pi returns undefined).
#[test]
fn no_servers_means_no_section() {
    assert_eq!(render_servers_section(&[]), None);
    let disabled = vec![McpServerListing {
        name: "off",
        description: Some("d"),
        instructions: None,
        reach: ServerReach::Codemode,
        enabled: false,
    }];
    assert_eq!(render_servers_section(&disabled), None);
}

// ---- exposure config (OQ3 item 5) ------------------------------------------

/// pi `getMcpToolExposure` (`mcp-servers.ts:170-178`): exact override wins,
/// then the first matching `*` pattern, then the server default.
#[test]
fn exposure_resolution_exact_then_first_pattern_then_default() {
    let overrides = vec![
        ("web_*".to_string(), ToolExposure::Hidden),
        ("*_old".to_string(), ToolExposure::Direct),
        ("read".to_string(), ToolExposure::Direct),
    ];
    assert_eq!(
        get_tool_exposure(Some(ToolExposure::Codemode), &overrides, "read"),
        ToolExposure::Direct
    );
    assert_eq!(
        get_tool_exposure(Some(ToolExposure::Codemode), &overrides, "web_fetch"),
        ToolExposure::Hidden
    );
    assert_eq!(
        get_tool_exposure(Some(ToolExposure::Direct), &overrides, "cache_old"),
        ToolExposure::Direct
    );
    assert_eq!(
        get_tool_exposure(Some(ToolExposure::Codemode), &overrides, "unrelated"),
        ToolExposure::Codemode
    );
    assert_eq!(
        get_tool_exposure(None, &[], "anything"),
        ToolExposure::Codemode
    );
    assert_eq!(ToolExposure::Deferred.as_str(), "deferred");
    assert_eq!(
        ToolExposure::parse("deferred"),
        Some(ToolExposure::Deferred)
    );
    assert_eq!(ToolExposure::parse("bogus"), None);
    // Wildcard semantics: `*` is an in-order multi-part wildcard.
    assert!(tool_pattern_matches("web_*", "web_fetch"));
    assert!(tool_pattern_matches("*_old", "cache_old"));
    assert!(tool_pattern_matches("*fetch*", "web_fetch_now"));
    assert!(tool_pattern_matches("a*b*c", "a-x-b-y-c"));
    assert!(!tool_pattern_matches("a*b*c", "a-x-c-b"));
    assert!(!tool_pattern_matches("web_*", "notweb_fetch"));
    assert!(tool_pattern_matches("exact", "exact"));
    assert!(!tool_pattern_matches("exact", "other"));
}
