//! TypeScript declaration rendering for the script-visible API: a line-for-line
//! Rust mirror of pi's `packages/codemode/src/declarations.ts`. Tools become
//! members of `declare const tools`, globals become `declare function`
//! statements, and `ns.member` globals become members of `declare const ns`.
//! Descriptions become doc comments; schemas become types via [`schema_to_type`].
//!
//! Host-side only: the runtime prelude never sees rendered types — its
//! `toolsJson` carries `{ name, jsName, description }` (pi:
//! `runtime/prelude-source.ts:22-23`). This module feeds the codemode tool
//! description and per-tool samples (SPEC §7, D9).

use std::collections::{BTreeSet, HashMap, HashSet};

use serde_json::Value;

use crate::{
    naming::to_codemode_identifier,
    types::{JsonSchema, Tool},
};

/// `^[A-Za-z_$][A-Za-z0-9_$]*$` — keys matching render bare; others JSON-quoted.
fn is_identifier_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn is_identifier_part(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

const INDENT: &str = "  ";
/// Largest rendered input type, in characters, before it becomes `unknown`.
pub const DEFAULT_INPUT_SCHEMA_MAX_CHARS: usize = 16_000;
/// Local `$ref` expansions per rendered schema, so shared definitions cannot blow up the output.
const MAX_REF_EXPANSIONS: usize = 32;
/// JSON Schema keywords with a number or string value shown as `@keyword value` property tags.
const TAG_KEYWORDS: [&str; 11] = [
    "minimum",
    "exclusiveMinimum",
    "maximum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "pattern",
    "format",
    "minItems",
    "maxItems",
];
/// Tag keywords whose values all hold at once, so only the largest (lower) or smallest (upper) matters.
const LOWER_BOUNDS: [&str; 4] = ["minimum", "exclusiveMinimum", "minLength", "minItems"];
const UPPER_BOUNDS: [&str; 4] = ["maximum", "exclusiveMaximum", "maxLength", "maxItems"];
/// Longest property tag; longer ones (large defaults, long patterns) are left out.
const MAX_TAG_CHARS: usize = 80;

/// Tools and globals to render. (pi: `RenderDeclarationsOptions`.)
pub struct Declarations<'a> {
    pub tools: &'a [Tool],
    pub globals: &'a [Tool],
}

/// Render TypeScript declarations for the script-visible API. Tools become members of
/// `declare const tools`, globals become `declare function` statements, and `ns.member` globals
/// become members of `declare const ns`. Descriptions become doc comments; schemas become types.
pub fn render_declarations(options: &Declarations<'_>) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !options.tools.is_empty() {
        let members = options
            .tools
            .iter()
            .map(|tool| {
                format!(
                    "{}{}{}",
                    doc_comment(tool.description.as_deref(), INDENT),
                    INDENT,
                    render_tool_signature(tool, None),
                )
            })
            .collect::<Vec<_>>();
        sections.push(format!(
            "declare const tools: {{\n{}\n}};",
            members.join("\n")
        ));
    }
    let mut namespaces: Vec<(String, Vec<String>)> = Vec::new();
    let mut namespace_index: HashMap<String, usize> = HashMap::new();
    for global in options.globals {
        let Some(dot) = global.name.find('.') else {
            sections.push(render_global(
                &format!("declare function {}", global.name),
                global,
                "",
            ));
            continue;
        };
        let namespace = global.name[..dot].to_string();
        let index = match namespace_index.get(&namespace) {
            Some(&index) => index,
            None => {
                namespaces.push((namespace.clone(), Vec::new()));
                let index = namespaces.len() - 1;
                namespace_index.insert(namespace, index);
                index
            }
        };
        namespaces[index]
            .1
            .push(render_global(&global.name[dot + 1..], global, INDENT));
    }
    for (namespace, members) in &namespaces {
        sections.push(format!(
            "declare const {namespace}: {{\n{}\n}};",
            members.join("\n")
        ));
    }
    sections.join("\n\n")
}

/// One tool as a member of the `tools` object: `name(args: T): Promise<R>;` with the
/// name as the identifier scripts use. Input types longer than `input_max_chars` render as `unknown`.
pub fn render_tool_signature(tool: &Tool, input_max_chars: Option<usize>) -> String {
    let input = match &tool.input_schema {
        None => "unknown".to_string(),
        Some(schema) => schema_to_type(
            schema,
            Some(input_max_chars.unwrap_or(DEFAULT_INPUT_SCHEMA_MAX_CHARS)),
        ),
    };
    format!(
        "{}(args: {input}): Promise<{}>;",
        to_codemode_identifier(&tool.name),
        render_tool_output_type(tool.output_schema.as_ref()),
    )
}

/// A tool's sample: the description followed by the tool's declaration. Used for tool
/// listings and `ALL_TOOLS` entries.
pub fn render_tool_sample(tool: &Tool, input_max_chars: Option<usize>) -> String {
    let declaration = format!(
        "declare const tools: {{ {} }};",
        render_tool_signature(tool, input_max_chars)
    );
    format!(
        "{}\n\ncodemode tool declaration:\n```ts\n{declaration}\n```",
        tool.description.as_deref().unwrap_or("").trim(),
    )
}

/// The type a tool call resolves to: the output schema's type, and `unknown` without a schema.
pub fn render_tool_output_type(schema: Option<&JsonSchema>) -> String {
    match schema {
        None => "unknown".to_string(),
        Some(schema) => schema_to_type(schema, None),
    }
}

fn render_global(head: &str, global: &Tool, indent: &str) -> String {
    if let Some(signature) = &global.signature {
        return format!(
            "{}{}{head}{signature};",
            doc_comment(global.description.as_deref(), indent),
            indent,
        );
    }
    let input = match &global.input_schema {
        None => "unknown".to_string(),
        Some(schema) => schema_to_type(schema, None),
    };
    let output = match &global.output_schema {
        None => "unknown".to_string(),
        Some(schema) => schema_to_type(schema, None),
    };
    format!(
        "{}{}{head}(args: {input}): Promise<{output}>;",
        doc_comment(global.description.as_deref(), indent),
        indent,
    )
}

fn doc_comment(description: Option<&str>, indent: &str) -> String {
    let Some(text) = description.map(str::trim).filter(|text| !text.is_empty()) else {
        return String::new();
    };
    let text = text.replace("*/", "*\\/");
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .collect();
    if lines.len() == 1 {
        return format!("{indent}/** {} */\n", lines[0]);
    }
    let body = lines
        .iter()
        .map(|line| {
            if line.is_empty() {
                format!("{indent} *")
            } else {
                format!("{indent} * {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{indent}/**\n{body}\n{indent} */\n")
}

fn property_key(name: &str) -> String {
    let bytes = name.as_bytes();
    let identifier = !bytes.is_empty()
        && is_identifier_start(bytes[0])
        && bytes[1..].iter().all(|&b| is_identifier_part(b));
    if identifier {
        name.to_string()
    } else {
        serde_json::to_string(name).unwrap_or_else(|_| "null".into())
    }
}

fn union(types: Vec<String>) -> String {
    let mut unique: Vec<String> = Vec::new();
    for item in types {
        if !unique.contains(&item) {
            unique.push(item);
        }
    }
    if unique.iter().any(|item| item == "unknown") {
        return "unknown".into();
    }
    if unique.is_empty() {
        return "never".into();
    }
    unique.join(" | ")
}

/// Convert a JSON Schema to a TypeScript type expression: objects on one line (`{ a: string; b?: number; }`) with properties sorted by name,
/// or one property per line with `//` comments when a property has a description or constraints the
/// type cannot express (`// @integer @minimum 1 @default 20`); `Array<T>` for arrays. Local references
/// (`#/$defs/...`, `#/definitions/...`) resolve against `schema`; recursive and remote references
/// render as `unknown`. A result longer than `max_chars` is rendered again without the constraint tags,
/// and renders as `unknown` if it is still too long.
pub fn schema_to_type(schema: &JsonSchema, max_chars: Option<usize>) -> String {
    let render = |tags: bool| {
        let mut context = SchemaContext {
            root: schema,
            resolving: HashSet::new(),
            expansions: 0,
            tags,
        };
        to_type(schema, &mut context)
    };
    let kind = render(true);
    if max_chars.is_none() || kind.len() <= max_chars.unwrap() {
        return kind;
    }
    let untagged = render(false);
    if untagged.len() <= max_chars.unwrap() {
        untagged
    } else {
        "unknown".into()
    }
}

struct SchemaContext<'a> {
    root: &'a JsonSchema,
    /// References being expanded on the current path, to stop at recursive types.
    resolving: HashSet<String>,
    expansions: usize,
    /// Whether property comments include constraint tags.
    tags: bool,
}

/// The subschema a local `$ref` (`#`, `#/$defs/Item`) names in `root`, or `None` for remote references and
/// references that name nothing. Pointer segments are URI-decoded and JSON Pointer unescaped (`~1` is `/`, `~0` is `~`).
pub fn resolve_schema_ref<'a>(reference: &str, root: &'a Value) -> Option<&'a Value> {
    if reference != "#" && !reference.starts_with("#/") {
        return None;
    }
    let mut current = root;
    let pointer = reference.get(2..).unwrap_or("");
    for segment in pointer.split('/').filter(|s| !s.is_empty()) {
        let decoded = percent_decode(segment)?;
        let key = decoded.replace("~1", "/").replace("~0", "~");
        let Value::Object(map) = current else {
            return None;
        };
        current = map.get(&key)?;
    }
    Some(current)
}

/// `decodeURIComponent` for one JSON Pointer segment: a malformed escape
/// (`%A` alone, non-hex digits) or invalid UTF-8 names nothing instead of
/// failing the caller — schemas come from tools and MCP servers.
fn percent_decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes.get(index + 1..index + 3)?;
            let high = (hex[0] as char).to_digit(16)?;
            let low = (hex[1] as char).to_digit(16)?;
            out.push((high * 16 + low) as u8);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn as_object(schema: &Value) -> Option<&serde_json::Map<String, Value>> {
    match schema {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

fn to_type(schema: &JsonSchema, context: &mut SchemaContext<'_>) -> String {
    let Value::Bool(value) = schema else {
        let Some(map) = as_object(schema) else {
            return "unknown".into();
        };
        return to_type_object(map, context);
    };
    match value {
        true => "unknown".into(),
        false => "never".into(),
    }
}

fn to_type_object(map: &serde_json::Map<String, Value>, context: &mut SchemaContext<'_>) -> String {
    if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
        if context.resolving.contains(reference) || context.expansions >= MAX_REF_EXPANSIONS {
            return "unknown".into();
        }
        let Some(target) = resolve_schema_ref(reference, context.root) else {
            return "unknown".into();
        };
        context.expansions += 1;
        context.resolving.insert(reference.to_string());
        let rendered = to_type(target, context);
        context.resolving.remove(reference);
        return rendered;
    }

    if map.contains_key("const") {
        return json_stringify(map["const"].clone());
    }
    if let Some(values) = map.get("enum").and_then(Value::as_array) {
        return union(values.iter().cloned().map(json_stringify).collect());
    }

    let variants: Option<&Vec<Value>> =
        if let Some(any_of) = map.get("anyOf").and_then(Value::as_array) {
            Some(any_of)
        } else {
            map.get("oneOf").and_then(Value::as_array)
        };
    if let Some(variants) = variants {
        return union(
            variants
                .iter()
                .map(|variant| to_type(variant, context))
                .collect(),
        );
    }
    if let Some(all_of) = map.get("allOf").and_then(Value::as_array) {
        let parts: Vec<String> = all_of
            .iter()
            .map(|part| to_type(part, context))
            .filter(|part| part != "unknown")
            .collect();
        if parts.is_empty() {
            return "unknown".into();
        }
        return parts
            .iter()
            .map(|part| {
                if part.contains(" | ") {
                    format!("({part})")
                } else {
                    part.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" & ");
    }

    match map.get("type") {
        Some(Value::Array(types)) => {
            return union(
                types
                    .iter()
                    .map(|entry| {
                        let mut single = map.clone();
                        single.insert("type".into(), entry.clone());
                        to_type(&Value::Object(single), context)
                    })
                    .collect(),
            );
        }
        Some(Value::String(kind)) => {
            return match kind.as_str() {
                "string" => "string".into(),
                "number" | "integer" => "number".into(),
                "boolean" => "boolean".into(),
                "null" => "null".into(),
                "array" => array_type(map, context),
                "object" => object_type(map, context),
                _ => "unknown".into(),
            };
        }
        Some(_) => return "unknown".into(),
        None => {}
    }
    // No `type`: infer from the keywords present.
    if map.contains_key("properties")
        || map.contains_key("additionalProperties")
        || map.contains_key("required")
    {
        return object_type(map, context);
    }
    if map.contains_key("items") || map.contains_key("prefixItems") {
        return array_type(map, context);
    }
    "unknown".into()
}

fn array_type(map: &serde_json::Map<String, Value>, context: &mut SchemaContext<'_>) -> String {
    if let Some(items) = map.get("items")
        && !items.is_array()
    {
        return format!("Array<{}>", to_type(items, context));
    }
    let tuple = map
        .get("prefixItems")
        .and_then(Value::as_array)
        .or_else(|| map.get("items").and_then(Value::as_array));
    if let Some(tuple) = tuple
        && !tuple.is_empty()
    {
        let items = tuple
            .iter()
            .map(|item| to_type(item, context))
            .collect::<Vec<_>>()
            .join(", ");
        return format!("[{items}]");
    }
    "unknown[]".into()
}

fn description_of(property: &Value) -> String {
    property
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// Lines of a description split on line terminators (`\r`, `\n`, U+2028, U+2029),
/// trimmed, empties dropped.
fn description_lines(text: &str) -> Vec<String> {
    text.split(['\r', '\n', '\u{2028}', '\u{2029}'])
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

/// Escape line terminators in a tag as `\uXXXX` (lowercase, zero padded).
fn escape_line_terminators(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\r' | '\n' | '\u{2028}' | '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", ch as u32));
            }
            other => out.push(other),
        }
    }
    out
}

/// A schema keyword as a number or string, for tag values.
fn keyword_value(schema: &Value, keyword: &str) -> Option<Value> {
    schema.get(keyword).and_then(|value| match value {
        Value::Number(_) | Value::String(_) => Some(value.clone()),
        _ => None,
    })
}

/// JS `String(number)`: integral floats print without a fraction (`String(1.0) === "1"`).
fn js_number_string(value: &Value) -> String {
    let Some(number) = value.as_number() else {
        return value.as_str().unwrap_or("").to_string();
    };
    if let Some(int) = number.as_i64() {
        return int.to_string();
    }
    if let Some(uint) = number.as_u64() {
        return uint.to_string();
    }
    let float = number.as_f64().unwrap_or(0.0);
    if float.fract() == 0.0 && float.abs() < 1e21 {
        format!("{float:.0}")
    } else {
        format!("{float}")
    }
}

/// Constraints of a property that its TypeScript type cannot express, as JSDoc-style tags:
/// `@integer @minimum 1 @maximum 100 @default 20`. Empty when there are none.
fn constraint_tags(property: &Value) -> String {
    let Value::Object(map) = property else {
        return String::new();
    };
    // Optional fields such as Pydantic's `int | None` keep their constraints in the
    // non-null branch. The branch's constraints apply together with the property's,
    // so both are read.
    let variants = map
        .get("anyOf")
        .and_then(Value::as_array)
        .or_else(|| map.get("oneOf").and_then(Value::as_array));
    let non_null: Vec<&Value> = variants
        .map(|variants| {
            variants
                .iter()
                .filter(|variant| variant.get("type").and_then(Value::as_str) != Some("null"))
                .collect()
        })
        .unwrap_or_default();
    let branch = if non_null.len() == 1 {
        Some(non_null[0])
    } else {
        None
    };
    let branches: Vec<&Value> = match branch {
        Some(branch) if branch.is_object() => vec![property, branch],
        _ => vec![property],
    };
    // Draft-04 and OpenAPI 3.0 make `minimum`/`maximum` exclusive with a boolean
    // `exclusiveMinimum`/`exclusiveMaximum`: fold the boolean onto the bound itself.
    let schemas: Vec<serde_json::Map<String, Value>> = branches
        .iter()
        .map(|schema| {
            let Value::Object(map) = schema else {
                return serde_json::Map::new();
            };
            let mut rewritten = map.clone();
            if map.get("exclusiveMinimum") == Some(&Value::Bool(true)) {
                rewritten.remove("minimum");
                match map.get("minimum") {
                    Some(bound @ Value::Number(_)) => {
                        rewritten.insert("exclusiveMinimum".into(), bound.clone());
                    }
                    _ => {
                        rewritten.remove("exclusiveMinimum");
                    }
                }
            }
            if map.get("exclusiveMaximum") == Some(&Value::Bool(true)) {
                rewritten.remove("maximum");
                match map.get("maximum") {
                    Some(bound @ Value::Number(_)) => {
                        rewritten.insert("exclusiveMaximum".into(), bound.clone());
                    }
                    _ => {
                        rewritten.remove("exclusiveMaximum");
                    }
                }
            }
            rewritten
        })
        .collect();

    let mut tags: Vec<String> = Vec::new();
    let is_integer = |schema: &serde_json::Map<String, Value>| match schema.get("type") {
        Some(Value::String(kind)) => kind == "integer",
        Some(Value::Array(kinds)) => kinds
            .iter()
            .any(|kind| kind == &Value::String("integer".into())),
        _ => false,
    };
    if schemas.iter().any(is_integer) {
        tags.push("@integer".into());
    }
    for keyword in TAG_KEYWORDS {
        let values: Vec<Option<Value>> = schemas
            .iter()
            .map(|schema| keyword_value(&Value::Object(schema.clone()), keyword))
            .collect();
        let bounded = LOWER_BOUNDS.contains(&keyword) || UPPER_BOUNDS.contains(&keyword);
        if bounded {
            let numbers: Vec<f64> = values
                .iter()
                .flatten()
                .filter_map(|value| value.as_f64())
                .collect();
            if numbers.is_empty() {
                continue;
            }
            let bound = if LOWER_BOUNDS.contains(&keyword) {
                numbers.iter().cloned().fold(f64::NAN, f64::max)
            } else {
                numbers.iter().cloned().fold(f64::NAN, f64::min)
            };
            tags.push(format!(
                "@{keyword} {}",
                js_number_string(&serde_json::json!(bound))
            ));
            continue;
        }
        // Only the first occurrence of each distinct value is kept (pi: `new Set(values)`).
        let mut seen: Vec<Value> = Vec::new();
        for value in values.into_iter().flatten() {
            if !seen.contains(&value) {
                seen.push(value.clone());
                tags.push(format!("@{keyword} {}", js_number_string(&value)));
            }
        }
    }
    if schemas
        .iter()
        .any(|schema| schema.get("uniqueItems") == Some(&Value::Bool(true)))
    {
        tags.push("@uniqueItems".into());
    }
    // Annotations, unlike constraints, come from the property itself.
    if let Some(default) = map.get("default")
        && !default.is_null()
    {
        tags.push(format!("@default {}", json_stringify(default.clone())));
    }
    if map.get("deprecated") == Some(&Value::Bool(true)) {
        tags.push("@deprecated".into());
    }
    tags.into_iter()
        .filter(|tag| tag.len() <= MAX_TAG_CHARS)
        .map(|tag| escape_line_terminators(&tag))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `JSON.stringify` of a schema value; `undefined` values (impossible in
/// serde_json) would fall back to `"unknown"` in pi.
fn json_stringify(value: Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "unknown".into())
}

fn object_type(map: &serde_json::Map<String, Value>, context: &mut SchemaContext<'_>) -> String {
    let properties = map
        .get("properties")
        .and_then(|properties| as_object(properties))
        .cloned()
        .unwrap_or_default();
    let required: BTreeSet<String> = map
        .get("required")
        .and_then(Value::as_array)
        .map(|required| {
            required
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    // pi: `Object.keys(properties).sort()` — lexicographic.
    let mut names: Vec<&String> = properties.keys().collect();
    names.sort();
    let members: Vec<String> = names
        .iter()
        .map(|name| {
            let optional = if required.contains(*name) { "" } else { "?" };
            format!(
                "{}{optional}: {};",
                property_key(name),
                to_type(&properties[*name], context)
            )
        })
        .collect();
    let additional = map.get("additionalProperties");
    let mut extra_members: Vec<String> = Vec::new();
    match additional {
        Some(Value::Bool(false)) | None => {
            if additional.is_none() && names.is_empty() {
                extra_members.push("[key: string]: unknown;".into());
            }
        }
        Some(Value::Bool(true)) => {
            extra_members.push("[key: string]: unknown;".into());
        }
        Some(schema) => {
            extra_members.push(format!("[key: string]: {};", to_type(schema, context)));
        }
    }
    if members.is_empty() && extra_members.is_empty() {
        return "{}".into();
    }
    let comments: Vec<Vec<String>> = names
        .iter()
        .map(|name| {
            let lines = description_lines(&description_of(&properties[*name]));
            let tags = if context.tags {
                constraint_tags(&properties[*name])
            } else {
                String::new()
            };
            if tags.is_empty() {
                lines
            } else {
                let mut with_tags = lines;
                with_tags.push(tags);
                with_tags
            }
        })
        .collect();
    if !comments.iter().any(|comment| !comment.is_empty()) {
        let mut all = members.clone();
        all.extend(extra_members.clone());
        return format!("{{ {} }}", all.join(" "));
    }

    let mut lines = vec!["{".to_string()];
    for (index, comment) in comments.iter().enumerate() {
        for line in comment {
            lines.push(format!("{INDENT}// {line}"));
        }
        lines.push(format!(
            "{INDENT}{}",
            members[index].replace('\n', &format!("\n{INDENT}"))
        ));
    }
    for member in &extra_members {
        lines.push(format!("{INDENT}{member}"));
    }
    lines.push("}".into());
    lines.join("\n")
}
