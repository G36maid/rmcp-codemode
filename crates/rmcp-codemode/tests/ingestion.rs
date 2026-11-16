//! §11.2 fixtures + §8 oracle: upstream ingestion, naming, and result
//! conversion against realistic MCP wire shapes. pi sources are labeled per
//! case (SPEC §8, D13, OQ4).

use rmcp_codemode::mcp::{
    MCP_OUTPUT_MAX_BYTES, McpContentBlock, ModelContent, UpstreamTool, call_tool_result_from_wire,
    canonical_tool_name, convert_mcp_result, ingest_tools, is_mcp_tool_name, mcp_namespace,
    mcp_server_summary, parse_tool_list, servers_conflict, validate_server_name,
};
use serde_json::{Value, json};

/// A realistic `tools/list` response shaped like GitHub's MCP server.
fn github_tool_list() -> Value {
    json!({
        "tools": [
            {
                "name": "search_repositories",
                "description": "Search GitHub repositories",
                "inputSchema": {
                    "type": "object",
                    "properties": { "query": { "type": "string", "description": "Search query" } },
                    "required": ["query"],
                },
                "annotations": { "readOnlyHint": true, "openWorldHint": true },
            },
            {
                "name": "create_issue",
                "description": "Create a new issue",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string" },
                        "body": { "type": "string" },
                    },
                    "required": ["title"],
                },
                "annotations": { "destructiveHint": false, "idempotentHint": false },
            },
            {
                "name": "get-file-contents",
                "description": "Get file contents; dashes normalize away",
                "inputSchema": { "type": "object", "properties": {} },
            },
        ],
    })
}

// ---- §8.1 naming (OQ4; pi packages/mcp/src/naming.ts) ----------------------

/// pi naming.ts: `createMcpToolName` — sanitize to `[A-Za-z0-9_]`.
#[test]
fn canonical_names_sanitize_to_identifier_charset() {
    assert_eq!(
        canonical_tool_name("github", "search_repositories", |_| false),
        "mcp__github__search_repositories"
    );
    assert_eq!(
        canonical_tool_name("chrome-devtools", "take_screenshot", |_| false),
        "mcp__chrome_devtools__take_screenshot"
    );
    // Everything outside letters/digits/underscore — including `.` — becomes `_`.
    assert_eq!(
        canonical_tool_name("docs", "read.file.txt", |_| false),
        "mcp__docs__read_file_txt"
    );
}

/// pi naming.ts: server names that differ only in `-`/`_` share a namespace,
/// so a configuration must not have both (SPEC §8.1).
#[test]
fn server_names_conflicting_only_on_dash_underscore_are_the_same_server() {
    assert!(servers_conflict("dev-radius", "dev_radius"));
    assert!(validate_server_name("dev-radius").is_ok());
    assert!(validate_server_name("dev_radius").is_ok());
    assert!(validate_server_name("web.search").is_err());
    assert!(validate_server_name("").is_err());
    assert_eq!(mcp_namespace("dev-radius"), "mcp__dev_radius");
}

/// pi naming.ts + index.ts:396-418 — sanitizing can map two tools to one
/// name; **all** colliding tools get the hash suffix, order-independently.
#[test]
fn colliding_tools_all_get_the_hash_suffix() {
    let tools = vec![
        UpstreamTool {
            name: "a-b".into(),
            ..UpstreamTool::default()
        },
        UpstreamTool {
            name: "a_b".into(),
            ..UpstreamTool::default()
        },
        UpstreamTool {
            name: "unrelated".into(),
            ..UpstreamTool::default()
        },
    ];
    let forward = ingest_tools("github", &tools);
    let reverse = ingest_tools(
        "github",
        &tools[1..]
            .iter()
            .chain(&tools[..1])
            .cloned()
            .collect::<Vec<_>>(),
    );
    let names = |ingested: &[rmcp_codemode::mcp::IngestedTool]| -> Vec<String> {
        let mut names: Vec<String> = ingested
            .iter()
            .map(|tool| tool.canonical_name.clone())
            .collect();
        names.sort();
        names
    };
    let forward_names = names(&forward);
    // Both `a-b` and `a_b` collide on `mcp__github__a_b` → BOTH are suffixed
    // (each with its own hash: the sha256 input is `server\0<original name>`),
    // and neither keeps the plain name.
    let colliding: Vec<&String> = forward_names
        .iter()
        .filter(|name| name.starts_with("mcp__github__a_b"))
        .collect();
    assert_eq!(colliding.len(), 2);
    assert_ne!(colliding[0], colliding[1], "distinct canonical names");
    assert_ne!(colliding[0], "mcp__github__a_b");
    assert_ne!(colliding[1], "mcp__github__a_b");
    let suffix_of = |name: &str| name.rsplit('_').next().unwrap_or("").to_string();
    assert_eq!(suffix_of(colliding[0]).len(), 8, "8-hex suffix");
    assert_eq!(suffix_of(colliding[1]).len(), 8);
    assert_ne!(suffix_of(colliding[0]), suffix_of(colliding[1]));
    // Order-independence: the same set either way.
    assert_eq!(forward_names, names(&reverse));
    // The non-colliding tool keeps its plain name.
    assert!(forward_names.contains(&"mcp__github__unrelated".to_string()));
    // Original names survive for call dispatch.
    assert_eq!(forward[0].original_name, "a-b");
    assert_eq!(forward[1].original_name, "a_b");
}

/// pi naming.ts: names over 64 characters shorten with the hash suffix.
#[test]
fn over_long_names_shorten_with_the_hash_suffix() {
    let long_tool = "x".repeat(80);
    let name = canonical_tool_name("github", &long_tool, |_| false);
    assert_eq!(name.len(), 64);
    assert!(name.starts_with("mcp__github__"));
    assert!(name.ends_with(&name[name.len() - 8..]), "{name}");
    // The suffix is the sha256 prefix of `server\0tool`.
    assert_eq!(
        canonical_tool_name("github", &long_tool, |_| false),
        name,
        "stable"
    );
}

/// pi naming.ts: `mcpServerSummary` — first line of the description, else of
/// the initialize instructions; empty when neither (§7 `mcp_servers` rows).
#[test]
fn server_summary_prefers_description_over_instructions() {
    assert_eq!(
        mcp_server_summary(Some("GitHub tools.\nSecond line."), None),
        "GitHub tools."
    );
    assert_eq!(
        mcp_server_summary(None, Some("  From initialize.\nmore")),
        "From initialize."
    );
    assert_eq!(mcp_server_summary(Some("   "), None), "");
    assert_eq!(mcp_server_summary(None, None), "");
}

/// pi mcp-servers.ts:47-49: MCP tool names are `mcp__…` or the resource tools.
#[test]
fn mcp_tool_name_recognition() {
    assert!(is_mcp_tool_name("mcp__github__search_repositories"));
    assert!(is_mcp_tool_name("read_mcp_resource"));
    assert!(is_mcp_tool_name("list_mcp_resources"));
    assert!(!is_mcp_tool_name("codemode"));
    assert!(!is_mcp_tool_name("my_own_tool"));
}

// ---- §8.2 ingestion (D13) --------------------------------------------------

/// Tolerant parse of a realistic `tools/list` (GitHub-shaped fixture): names,
/// descriptions, schemas, and the four forwarded annotation hints. Icons and
/// `_meta` are stripped at parse time.
#[test]
fn ingests_a_realistic_tool_list_with_annotations() {
    let wire = github_tool_list();
    // Icons and _meta ride along on real servers; parse must ignore them.
    let mut wire = wire;
    wire["tools"][0]["icons"] = json!([{ "src": "https://example.com/icon.png" }]);
    wire["tools"][0]["_meta"] = json!({ "vendor/key": "value" });

    let tools = parse_tool_list(&wire);
    assert_eq!(tools.len(), 3);
    assert_eq!(tools[0].name, "search_repositories");
    assert_eq!(
        tools[0].description.as_deref(),
        Some("Search GitHub repositories")
    );
    let annotations = tools[0].annotations.as_ref().unwrap();
    assert_eq!(annotations.read_only, Some(true));
    assert_eq!(annotations.open_world, Some(true));
    assert_eq!(annotations.destructive, None);
    // tools[1]: only destructive+idempotent.
    let annotations = tools[1].annotations.as_ref().unwrap();
    assert_eq!(annotations.destructive, Some(false));
    assert_eq!(annotations.idempotent, Some(false));
    assert_eq!(annotations.read_only, None);
    // tools[2]: no annotations at all → None.
    assert_eq!(tools[2].annotations, None);
    assert_eq!(tools[2].name, "get-file-contents");

    let ingested = ingest_tools("github", &tools);
    assert_eq!(
        ingested[0].canonical_name,
        "mcp__github__search_repositories"
    );
    assert_eq!(ingested[2].canonical_name, "mcp__github__get_file_contents");
    assert_eq!(
        ingested[0].input_schema.as_ref().unwrap()["required"],
        json!(["query"])
    );
}

/// A server without the `tools` capability contributes no tool entries
/// (pi `index.ts:387`); a `tools: []` response is likewise empty.
#[test]
fn prompt_only_server_contributes_nothing() {
    assert!(parse_tool_list(&json!({ "prompts": [] })).is_empty());
    assert!(parse_tool_list(&json!({ "tools": [] })).is_empty());
    assert!(parse_tool_list(&json!({})).is_empty());
}

// ---- §8.2 result conversion (pi tools.ts:215-235, content.ts) --------------

/// pi `convertMcpResult`: isError resolves — **scripts receive the full
/// result** (content + structuredContent + isError), only the model-facing
/// side is marked. Non-empty error text stays as it is.
#[test]
fn is_error_resolves_to_scripts_with_full_result() {
    let wire = json!({
        "content": [{ "type": "text", "text": "The query was invalid: expected `org/repo`" }],
        "isError": true,
        "_meta": { "dropped": true },
    });
    let result = call_tool_result_from_wire(&wire);
    let converted = convert_mcp_result("github", "search_repositories", &result, false, None);
    // Scripts see everything except _meta.
    assert_eq!(
        converted.script_result,
        json!({
            "content": [{ "type": "text", "text": "The query was invalid: expected `org/repo`" }],
            "isError": true,
        })
    );
    assert!(converted.is_error);
    // Model-facing: the text, no synthesized line (it is non-empty).
    assert_eq!(
        converted.content,
        vec![ModelContent::Text {
            text: "The query was invalid: expected `org/repo`".into()
        }]
    );
}

/// pi `tools.ts:224-226`: an error result whose text is empty gets the
/// synthesized line appended.
#[test]
fn empty_error_text_gets_the_synthesized_line() {
    let wire = json!({ "content": [], "isError": true });
    let result = call_tool_result_from_wire(&wire);
    let converted = convert_mcp_result("github", "search_repositories", &result, false, None);
    assert_eq!(
        converted.content,
        vec![ModelContent::Text {
            text: "MCP tool github/search_repositories returned an error".into()
        }]
    );
}

/// pi `toLlmContent` (content.ts:112-118): no content blocks + structured
/// content → the structured content's pretty JSON. Tolerant: any JSON value.
#[test]
fn structured_content_without_blocks_becomes_pretty_json() {
    let wire = json!({ "content": [], "structuredContent": { "ok": true, "count": 2 } });
    let result = call_tool_result_from_wire(&wire);
    let converted = convert_mcp_result("github", "search_repositories", &result, false, None);
    assert_eq!(
        converted.content,
        vec![ModelContent::Text {
            text: "{\n  \"ok\": true,\n  \"count\": 2\n}".into()
        }]
    );
}

/// pi `blockToContent` (tools.ts:173-202): resource links render as a
/// bracketed description with the read hint only when resources are readable;
/// embedded text decodes; non-image blobs save through the host and binary
/// text-like blobs decode as UTF-8.
#[test]
fn resource_blocks_convert_with_read_hint_and_saved_blobs() {
    let wire = json!({
        "content": [
            { "type": "resource_link", "uri": "file:///repo/README.md", "name": "README",
              "mimeType": "text/markdown", "size": 1200, "description": "The readme" },
            { "type": "resource", "resource": { "uri": "file:///notes.txt", "text": "plain notes" } },
            { "type": "resource", "resource": { "uri": "file:///logo.png", "blob": "aGk=", "mimeType": "image/png" } },
        ],
    });
    let result = call_tool_result_from_wire(&wire);

    // Without readable resources: no read hint.
    let converted = convert_mcp_result("docs", "read", &result, false, None);
    assert_eq!(
        converted.content[0],
        ModelContent::Text {
            text: "[Resource file:///repo/README.md \"README\" (text/markdown, 1.2KB): The readme]"
                .into()
        }
    );
    // With readable resources: the read hint appears.
    let converted = convert_mcp_result("docs", "read", &result, true, None);
    let ModelContent::Text { text } = &converted.content[0] else {
        panic!("expected text");
    };
    assert!(
        text.contains(". Read it with read_mcp_resource (server \"docs\")"),
        "{text}"
    );
    // Embedded text decodes; embedded image passes through.
    assert_eq!(
        converted.content[1],
        ModelContent::Text {
            text: "plain notes".into()
        }
    );
    assert_eq!(
        converted.content[2],
        ModelContent::Image {
            data: "aGk=".into(),
            mime_type: "image/png".into()
        }
    );

    // A non-image blob saves through the host saver.
    let blob = json!({
        "content": [{ "type": "resource", "resource": { "uri": "file:///data.bin", "blob": "AAEC", "mimeType": "application/octet-stream" } }],
    });
    let result = call_tool_result_from_wire(&blob);
    let mut saved = String::new();
    let mut saver = |data: &str, extension: &str| -> Result<String, String> {
        saved = format!("{data}{extension}");
        Ok("/tmp/full.bin".into())
    };
    let converted = convert_mcp_result("docs", "read", &result, false, Some(&mut saver));
    assert_eq!(
        converted.content,
        vec![ModelContent::Text {
            text: "[Binary resource file:///data.bin (application/octet-stream, 3B) saved to /tmp/full.bin]".into()
        }]
    );
    assert_eq!(
        saved, "\0\x01\x02.bin",
        "decoded bytes and pi's extensionOf"
    );
}

/// pi `limitMcpContent` (output-limit.ts): text beyond 20 KiB is cut in the
/// middle with the exact warning header, full output goes to the saver, and
/// image blocks survive after the truncation notice.
#[test]
fn oversized_output_truncates_with_the_exact_pi_format() {
    let long = "x".repeat(MCP_OUTPUT_MAX_BYTES + 500);
    let wire = json!({
        "content": [
            { "type": "text", "text": long },
            { "type": "image", "data": "aGk=", "mimeType": "image/png" },
        ],
    });
    let result = call_tool_result_from_wire(&wire);
    let mut saved_len = 0usize;
    let mut saver = |data: &str, _extension: &str| -> Result<String, String> {
        saved_len = data.len();
        Ok("/tmp/full.txt".into())
    };
    let converted = convert_mcp_result(
        "github",
        "search_repositories",
        &result,
        false,
        Some(&mut saver),
    );

    let ModelContent::Text { text } = &converted.content[0] else {
        panic!("expected text");
    };
    assert_eq!(
        saved_len,
        MCP_OUTPUT_MAX_BYTES + 500,
        "the full text is saved"
    );
    assert!(converted.full_output_path.as_deref() == Some("/tmp/full.txt"));
    let total_bytes = MCP_OUTPUT_MAX_BYTES + 500;
    let expected_header = format!(
        "Warning: truncated output (original token count: {})\nTotal output lines: 1\n\n",
        total_bytes.div_ceil(4)
    );
    assert!(text.starts_with(&expected_header), "{text:?}");
    assert!(text.contains("chars truncated…"), "{text:?}");
    assert!(text.ends_with("\n\n[Full output: /tmp/full.txt (read it with offset/limit)]"));
    assert_eq!(
        converted.content[1],
        ModelContent::Image {
            data: "aGk=".into(),
            mime_type: "image/png".into()
        },
        "images follow the truncation notice"
    );
    assert_eq!(converted.content.len(), 2);
}

/// Within the limit, content passes through unchanged and nothing is saved.
#[test]
fn output_within_the_limit_passes_through() {
    let wire = json!({ "content": [{ "type": "text", "text": "short" }] });
    let result = call_tool_result_from_wire(&wire);
    let mut save_called = false;
    let mut saver = |_: &str, _: &str| -> Result<String, String> {
        save_called = true;
        Ok("/tmp/never".into())
    };
    let converted = convert_mcp_result(
        "github",
        "search_repositories",
        &result,
        false,
        Some(&mut saver),
    );
    assert!(!save_called);
    assert_eq!(converted.full_output_path, None);
    assert_eq!(
        converted.content,
        vec![ModelContent::Text {
            text: "short".into()
        }]
    );
}

/// Unknown content block types still convert (placeholder), and an unknown
/// wire shape parses to the unsupported fallback (tolerance).
#[test]
fn unknown_block_types_are_tolerated() {
    let wire = json!({ "content": [{ "type": "mcp_app_render", "payload": {} }] });
    let result = call_tool_result_from_wire(&wire);
    assert_eq!(
        result.content,
        vec![McpContentBlock::Unsupported {
            kind: "mcp_app_render".into()
        }]
    );
    let converted = convert_mcp_result("ui", "render", &result, false, None);
    assert_eq!(
        converted.content,
        vec![ModelContent::Text {
            text: "[unsupported MCP content mcp_app_render]".into()
        }]
    );
}
