//! Oracle suite: pi's `packages/codemode/test/declarations.test.ts` ported
//! mechanically (same fixtures, same assertions). 19 cases in four describes.

use std::sync::Arc;

use rmcp_codemode::{
    Declarations, Tool, ToolContext, ToolFn, render_declarations, render_tool_sample,
    render_tool_signature, resolve_schema_ref, schema_to_type,
};
use serde_json::{Value, json};

/// pi test file: `const execute = () => undefined;`
fn execute() -> ToolFn {
    Arc::new(|_args: Value, _context: ToolContext| {
        Box::pin(async {
            Err(rmcp_codemode::ToolError {
                message: "unused".into(),
            })
        })
    })
}

fn tool(name: &str) -> Tool {
    Tool {
        name: name.into(),
        description: None,
        input_schema: None,
        output_schema: None,
        spread: false,
        signature: None,
        execute: execute(),
    }
}

// ---- describe("schemaToType") ---------------------------------------------

/// pi: `renders primitives, literals, and unions`
#[test]
fn renders_primitives_literals_and_unions() {
    assert_eq!(schema_to_type(&json!({ "type": "string" }), None), "string");
    assert_eq!(
        schema_to_type(&json!({ "type": "integer" }), None),
        "number"
    );
    assert_eq!(
        schema_to_type(&json!({ "type": ["string", "null"] }), None),
        "string | null"
    );
    assert_eq!(schema_to_type(&json!({ "const": "a" }), None), "\"a\"");
    assert_eq!(
        schema_to_type(&json!({ "enum": ["a", 1, null] }), None),
        "\"a\" | 1 | null"
    );
    assert_eq!(
        schema_to_type(
            &json!({ "anyOf": [{ "type": "string" }, { "type": "number" }] }),
            None
        ),
        "string | number"
    );
    assert_eq!(
        schema_to_type(&json!({ "anyOf": [{ "type": "string" }, {}] }), None),
        "unknown"
    );
    assert_eq!(
        schema_to_type(
            &json!({ "allOf": [{ "anyOf": [{ "type": "string" }, { "type": "number" }] }, { "const": 1 }] }),
            None
        ),
        "(string | number) & 1"
    );
    assert_eq!(
        schema_to_type(&json!({ "$ref": "#/defs/x" }), None),
        "unknown"
    );
    assert_eq!(schema_to_type(&Value::Bool(true), None), "unknown");
    assert_eq!(schema_to_type(&Value::Bool(false), None), "never");
}

/// pi: `renders objects on one line with sorted properties`
#[test]
fn renders_objects_on_one_line_with_sorted_properties() {
    assert_eq!(
        schema_to_type(
            &json!({
                "type": "object",
                "properties": { "city": { "type": "string" }, "max-lines": { "type": "number" } },
                "required": ["city"],
                "additionalProperties": false,
            }),
            None
        ),
        "{ city: string; \"max-lines\"?: number; }"
    );
    assert_eq!(
        schema_to_type(
            &json!({ "type": "object", "additionalProperties": { "type": "number" } }),
            None
        ),
        "{ [key: string]: number; }"
    );
    assert_eq!(
        schema_to_type(&json!({ "type": "object" }), None),
        "{ [key: string]: unknown; }"
    );
    assert_eq!(
        schema_to_type(
            &json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            None
        ),
        "{}"
    );
}

/// pi: `puts property descriptions on comment lines`
#[test]
fn puts_property_descriptions_on_comment_lines() {
    assert_eq!(
        schema_to_type(
            &json!({
                "type": "object",
                "properties": {
                    "weather": {
                        "type": "array",
                        "description": "look up weather for a given list of locations",
                        "items": { "type": "object", "properties": { "location": { "type": "string" } }, "required": ["location"] },
                    },
                },
                "required": ["weather"],
            }),
            None
        ),
        "{\n  // look up weather for a given list of locations\n  weather: Array<{ location: string; }>;\n}"
    );
    assert_eq!(
        schema_to_type(
            &json!({
                "type": "object",
                "properties": {
                    "outer": {
                        "type": "object",
                        "description": "Outer",
                        "properties": { "inner": { "type": "string", "description": "Inner" } },
                    },
                },
            }),
            None
        ),
        "{\n  // Outer\n  outer?: {\n    // Inner\n    inner?: string;\n  };\n}"
    );
}

/// pi: `puts constraints the type cannot express on property comment lines`
/// (https://github.com/earendil-works/pi/issues/10707)
#[test]
fn puts_constraints_on_property_comment_lines() {
    assert_eq!(
        schema_to_type(
            &json!({
                "type": "object",
                "properties": {
                    "timeoutMs": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 30000,
                        "default": 0,
                        "description": "Wait in milliseconds.",
                    },
                    "id": { "type": "string", "pattern": "^[a-z]+$", "maxLength": 8 },
                    "tags": { "type": "array", "items": { "type": "string" }, "maxItems": 5, "uniqueItems": true, "deprecated": true },
                    "name": { "type": "string" },
                },
                "required": ["id"],
            }),
            None
        ),
        [
            "{",
            "  // @maxLength 8 @pattern ^[a-z]+$",
            "  id: string;",
            "  name?: string;",
            "  // @maxItems 5 @uniqueItems @deprecated",
            "  tags?: Array<string>;",
            "  // Wait in milliseconds.",
            "  // @integer @minimum 0 @maximum 30000 @default 0",
            "  timeoutMs?: number;",
            "}",
        ]
        .join("\n")
    );
}

/// pi: `reads constraints of optional fields from the non-null branch`
#[test]
fn reads_constraints_of_optional_fields_from_the_non_null_branch() {
    // Pydantic's `limit: int | None = Field(None, ge=1, le=100)`.
    assert_eq!(
        schema_to_type(
            &json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "anyOf": [{ "type": "integer", "minimum": 1, "maximum": 100 }, { "type": "null" }],
                        "default": null,
                        "title": "Limit",
                    },
                },
            }),
            None
        ),
        "{\n  // @integer @minimum 1 @maximum 100\n  limit?: number | null;\n}"
    );
    // Constraints of the property and the branch both apply.
    assert_eq!(
        schema_to_type(
            &json!({
                "type": "object",
                "properties": {
                    "n": {
                        "anyOf": [{ "type": "integer", "minimum": 10, "maximum": 50, "format": "int32" }, { "type": "null" }],
                        "minimum": 0,
                        "maximum": 20,
                        "format": "port",
                    },
                },
            }),
            None
        ),
        "{\n  // @integer @minimum 10 @maximum 20 @format port @format int32\n  n?: number | null;\n}"
    );
}

/// pi: `reads draft-04 boolean exclusive bounds`
#[test]
fn reads_draft_04_boolean_exclusive_bounds() {
    assert_eq!(
        schema_to_type(
            &json!({
                "type": "object",
                "properties": {
                    "n": { "type": "integer", "minimum": 0, "exclusiveMinimum": true, "maximum": 10, "exclusiveMaximum": false },
                    "m": { "anyOf": [{ "type": "number", "maximum": 5, "exclusiveMaximum": true }, { "type": "null" }], "maximum": 8 },
                },
            }),
            None
        ),
        [
            "{",
            "  // @maximum 8 @exclusiveMaximum 5",
            "  m?: number | null;",
            "  // @integer @exclusiveMinimum 0 @maximum 10",
            "  n?: number;",
            "}",
        ]
        .join("\n")
    );
}

/// pi: `leaves out long tags and escapes line terminators`
#[test]
fn leaves_out_long_tags_and_escapes_line_terminators() {
    assert_eq!(
        schema_to_type(
            &json!({
                "type": "object",
                "properties": { "config": { "type": "object", "default": { "text": "x".repeat(100) } } },
            }),
            None
        ),
        "{ config?: { [key: string]: unknown; }; }"
    );
    assert_eq!(
        schema_to_type(
            &json!({
                "type": "object",
                "properties": {
                    "sep": {
                        "type": "string",
                        "pattern": "^[\r\n\u{2028}\u{2029}]$",
                        "default": "\u{2028}",
                        "description": "a\u{2028}b",
                    },
                },
            }),
            None
        ),
        [
            "{",
            "  // a",
            "  // b",
            "  // @pattern ^[\\u000d\\u000a\\u2028\\u2029]$ @default \"\\u2028\"",
            "  sep?: string;",
            "}",
        ]
        .join("\n")
    );
}

/// pi: `drops constraint tags before rendering a type over the budget as unknown`
#[test]
fn drops_constraint_tags_over_the_budget() {
    let schema =
        json!({ "type": "object", "properties": { "n": { "type": "integer", "minimum": 1 } } });
    assert_eq!(
        schema_to_type(&schema, None),
        "{\n  // @integer @minimum 1\n  n?: number;\n}"
    );
    assert_eq!(schema_to_type(&schema, Some(20)), "{ n?: number; }");
    assert_eq!(schema_to_type(&schema, Some(10)), "unknown");
}

/// pi: `resolves local references and stops at recursive ones`
#[test]
fn resolves_local_references_and_stops_at_recursive_ones() {
    let schema = json!({
        "type": "object",
        "properties": {
            "item": { "$ref": "#/$defs/Item" },
            "legacy": { "$ref": "#/definitions/Legacy" },
            "remote": { "$ref": "https://example.com/schema.json" },
        },
        "required": ["item"],
        "$defs": {
            "Item": {
                "type": "object",
                "properties": { "id": { "type": "string" }, "parent": { "$ref": "#/$defs/Item" } },
                "required": ["id"],
            },
        },
        "definitions": { "Legacy": { "enum": ["a", "b"] } },
    });
    assert_eq!(
        schema_to_type(&schema, None),
        "{ item: { id: string; parent?: unknown; }; legacy?: \"a\" | \"b\"; remote?: unknown; }"
    );
}

/// pi: `renders arrays and tuples`
#[test]
fn renders_arrays_and_tuples() {
    assert_eq!(
        schema_to_type(
            &json!({ "type": "array", "items": { "type": "string" } }),
            None
        ),
        "Array<string>"
    );
    assert_eq!(
        schema_to_type(
            &json!({ "type": "array", "prefixItems": [{ "type": "string" }, { "type": "number" }] }),
            None
        ),
        "[string, number]"
    );
    assert_eq!(
        schema_to_type(&json!({ "type": "array" }), None),
        "unknown[]"
    );
}

/// pi: `renders types over the budget as unknown`
#[test]
fn renders_types_over_the_budget_as_unknown() {
    let mut properties = serde_json::Map::new();
    for index in 0..50 {
        properties.insert(format!("field{index}"), json!({ "type": "string" }));
    }
    let schema = json!({ "type": "object", "properties": properties });
    assert_eq!(schema_to_type(&schema, Some(100)), "unknown");
    assert!(schema_to_type(&schema, None).contains("field49?: string;"));
}

// ---- describe("resolveSchemaRef") -----------------------------------------

fn ref_root() -> Value {
    json!({
        "$defs": {
            "item": { "type": "string" },
            "a/b": { "type": "string" },
            "c~d": { "type": "string" },
            "e f": { "type": "string" },
        },
    })
}

/// pi: `resolves local references with escaped segments`
#[test]
fn resolves_local_references_with_escaped_segments() {
    let root = ref_root();
    let item = &root["$defs"]["item"];
    assert_eq!(resolve_schema_ref("#", &root), Some(&root));
    assert_eq!(resolve_schema_ref("#/$defs/item", &root), Some(item));
    assert_eq!(resolve_schema_ref("#/$defs/a~1b", &root), Some(item));
    assert_eq!(resolve_schema_ref("#/$defs/c~0d", &root), Some(item));
    assert_eq!(resolve_schema_ref("#/$defs/e%20f", &root), Some(item));
}

/// pi: `returns undefined for remote, missing, and malformed references`
#[test]
fn returns_none_for_remote_missing_and_malformed_references() {
    let root = ref_root();
    assert_eq!(resolve_schema_ref("other.json#/$defs/item", &root), None);
    assert_eq!(resolve_schema_ref("#/$defs/missing", &root), None);
    assert_eq!(resolve_schema_ref("#/$defs/%E0%A4%A", &root), None);
}

// ---- describe("tool declarations") ----------------------------------------

/// pi: `renders signatures with normalized identifiers`
#[test]
fn renders_signatures_with_normalized_identifiers() {
    let mut hidden = tool("hidden-dynamic-tool");
    hidden.input_schema = Some(json!({
        "type": "object",
        "properties": { "city": { "type": "string" } },
        "required": ["city"],
        "additionalProperties": false,
    }));
    hidden.output_schema = Some(json!({
        "type": "object",
        "properties": { "ok": { "type": "boolean" } },
        "required": ["ok"],
    }));
    assert_eq!(
        render_tool_signature(&hidden, None),
        "hidden_dynamic_tool(args: { city: string; }): Promise<{ ok: boolean; }>;"
    );
    assert_eq!(
        render_tool_signature(&tool("free"), None),
        "free(args: unknown): Promise<unknown>;"
    );
}

/// pi: `renders output schemas shaped like an MCP CallToolResult as plain types`
#[test]
fn renders_output_schemas_shaped_like_mcp_call_tool_result() {
    let output_schema = json!({
        "type": "object",
        "properties": {
            "content": { "type": "array", "items": { "type": "object" } },
            "isError": { "type": "boolean" },
            "_meta": { "type": "object" },
        },
        "required": ["content"],
    });
    let mut plain = tool("plain");
    plain.input_schema = Some(json!({ "type": "object", "properties": {} }));
    plain.output_schema = Some(output_schema);
    assert_eq!(
        render_tool_signature(&plain, None),
        "plain(args: { [key: string]: unknown; }): Promise<{ _meta?: { [key: string]: unknown; }; content: Array<{ [key: string]: unknown; }>; isError?: boolean; }>;"
    );
}

/// pi: `renders the per-tool sample`
#[test]
fn renders_the_per_tool_sample() {
    let mut foo = tool("foo");
    foo.description = Some("bar".into());
    foo.input_schema = Some(json!({ "type": "string" }));
    assert_eq!(
        render_tool_sample(&foo, None),
        "bar\n\ncodemode tool declaration:\n```ts\ndeclare const tools: { foo(args: string): Promise<unknown>; };\n```"
    );
}

// ---- describe("renderDeclarations") ---------------------------------------

/// pi: `renders tools and globals`
#[test]
fn renders_tools_and_globals() {
    let mut read = tool("read");
    read.description = Some("Read a file.\nSecond line.".into());
    read.input_schema = Some(
        json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }),
    );
    read.output_schema = Some(json!({ "type": "string" }));
    let remote_api = tool("remote-api");

    let mut attach = tool("attach");
    attach.description = Some("Attach it.".into());
    attach.input_schema = Some(json!({ "type": "string" }));

    let text = render_declarations(&Declarations {
        tools: &[read, remote_api],
        globals: &[attach],
    });
    assert_eq!(
        text,
        [
            "declare const tools: {",
            "  /**",
            "   * Read a file.",
            "   * Second line.",
            "   */",
            "  read(args: { path: string; }): Promise<string>;",
            "  remote_api(args: unknown): Promise<unknown>;",
            "};",
            "",
            "/** Attach it. */",
            "declare function attach(args: string): Promise<unknown>;",
        ]
        .join("\n")
    );
}

/// pi: `renders namespaced globals and explicit signatures`
#[test]
fn renders_namespaced_globals_and_explicit_signatures() {
    let mut list = tool("models.list");
    list.description = Some("List models.".into());
    list.signature = Some("(type: string): Promise<string[]>".into());
    let mut get = tool("models.get");
    get.input_schema = Some(json!({ "type": "string" }));
    let mut plain = tool("plain");
    plain.signature = Some("(): void".into());

    let text = render_declarations(&Declarations {
        tools: &[],
        globals: &[list, get, plain],
    });
    assert_eq!(
        text,
        [
            "declare function plain(): void;",
            "",
            "declare const models: {",
            "  /** List models. */",
            "  list(type: string): Promise<string[]>;",
            "  get(args: string): Promise<unknown>;",
            "};",
        ]
        .join("\n")
    );
}

/// pi: `escapes comment terminators in descriptions`
#[test]
fn escapes_comment_terminators_in_descriptions() {
    let mut x = tool("x");
    x.description = Some("a */ b".into());
    let text = render_declarations(&Declarations {
        tools: &[x],
        globals: &[],
    });
    assert!(text.contains("/** a *\\/ b */"));
}
