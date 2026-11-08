//! TypeScript declaration rendering: JSON Schema → the signature the model
//! reads in `describeTool`, `describeNamespace`, and the inline catalog.
//!
//! pi's renderer scope: doc comments, required markers, enum/const literals,
//! `anyOf`/`oneOf` unions, arrays, nested objects, constraint tags
//! (`@minimum 1 @default 20`), and a max-length fallback to `unknown`.
//! Render-only: values are never validated against these schemas.

use serde_json::Value;

/// The largest rendered input type, in characters, before it becomes
/// `unknown` (pi's `DEFAULT_INPUT_SCHEMA_MAX_CHARS`).
pub const DEFAULT_SCHEMA_MAX_CHARS: usize = 16_000;

/// Render one tool's declaration:
/// `/** description */\ndeclare function name(args: Input): Promise<Output>;`
pub fn ts_declaration(
    name: &str,
    description: Option<&str>,
    input: Option<&Value>,
    output: Option<&Value>,
) -> String {
    let callable = name.rsplit('.').next().unwrap_or(name);
    let callable = crate::identity::normalize_identifier(callable);
    let input_type = match input {
        Some(schema) => schema_to_type(schema),
        None => "unknown".to_owned(),
    };
    let output_type = match output {
        Some(schema) => schema_to_type(schema),
        None => "unknown".to_owned(),
    };
    let declaration =
        format!("declare function {callable}(args: {input_type}): Promise<{output_type}>;");
    match description {
        Some(text) if !text.is_empty() => format!("/** {text} */\n{declaration}"),
        _ => declaration,
    }
}

/// Render a JSON Schema document as a TypeScript type expression.
pub fn schema_to_type(schema: &Value) -> String {
    schema_to_type_bounded(schema, DEFAULT_SCHEMA_MAX_CHARS)
}

/// [`schema_to_type`] with a max length: an over-long render retries without
/// constraint tags, then gives up as `unknown` (pi's rule).
pub fn schema_to_type_bounded(schema: &Value, max_chars: usize) -> String {
    let rendered = to_type(schema);
    if rendered.len() <= max_chars {
        return rendered;
    }
    // Without tags the render is shorter; if still too long, give up.
    let untagged = to_type(&strip_tags(schema));
    if untagged.len() <= max_chars {
        untagged
    } else {
        "unknown".to_owned()
    }
}

/// Remove constraint annotations that only affect the tag comments, so the
/// untagged retry is shorter.
fn strip_tags(schema: &Value) -> Value {
    let mut schema = schema.clone();
    if let Some(object) = schema.as_object_mut() {
        for key in [
            "minimum",
            "maximum",
            "exclusiveMinimum",
            "exclusiveMaximum",
            "multipleOf",
            "minLength",
            "maxLength",
            "pattern",
            "format",
            "minItems",
            "maxItems",
            "uniqueItems",
            "default",
        ] {
            object.remove(key);
        }
    }
    schema
}

fn to_type(schema: &Value) -> String {
    // const / enum literals first (pi: exact JSON spellings).
    if let Some(const_value) = schema.get("const") {
        return serde_json::to_string(const_value).unwrap_or_else(|_| "unknown".to_owned());
    }
    if let Some(variants) = schema.get("enum").and_then(Value::as_array) {
        let literals: Vec<String> = variants
            .iter()
            .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "unknown".to_owned()))
            .collect();
        if !literals.is_empty() {
            return literals.join(" | ");
        }
    }
    for combinator in ["anyOf", "oneOf"] {
        if let Some(variants) = schema.get(combinator).and_then(Value::as_array) {
            let rendered: Vec<String> = variants.iter().map(to_type).collect();
            if !rendered.is_empty() {
                return rendered.join(" | ");
            }
        }
    }
    if let Some(parts) = schema.get("allOf").and_then(Value::as_array) {
        let rendered: Vec<String> = parts
            .iter()
            .map(to_type)
            .filter(|part| part != "unknown")
            .map(|part| {
                if part.contains(" | ") {
                    format!("({part})")
                } else {
                    part
                }
            })
            .collect();
        if !rendered.is_empty() {
            return rendered.join(" & ");
        }
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => "string".to_owned(),
        Some("number" | "integer") => "number".to_owned(),
        Some("boolean") => "boolean".to_owned(),
        Some("null") => "null".to_owned(),
        Some("array") => match schema.get("items") {
            Some(items) => format!("Array<{}>", to_type(items)),
            None => "unknown[]".to_owned(),
        },
        Some("object") => object_type(schema),
        // No type but properties: an object (pi's undefined-type rule).
        None => {
            if schema.get("properties").is_some() || schema.get("required").is_some() {
                object_type(schema)
            } else {
                "unknown".to_owned()
            }
        }
        _ => "unknown".to_owned(),
    }
}

fn object_type(schema: &Value) -> String {
    let properties = match schema.get("properties").and_then(Value::as_object) {
        Some(properties) if !properties.is_empty() => properties,
        _ => return "Record<string, unknown>".to_owned(),
    };
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<std::collections::BTreeSet<_>>()
        })
        .unwrap_or_default();
    let mut members = Vec::new();
    for (property, property_schema) in properties {
        let optional = if required.contains(property) { "" } else { "?" };
        let member_type = to_type(property_schema);
        let member = format!("{property}{optional}: {member_type};");
        match property_schema
            .get("description")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
        {
            Some(doc) => members.push(format!("/** {doc} */ {member}")),
            None => members.push(member),
        }
    }
    if members.is_empty() {
        "{ [k: string]: unknown }".to_owned()
    } else {
        format!("{{ {} }}", members.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn renders_required_markers_and_doc_comments() {
        let schema = json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string", "description": "Repository owner" },
                "limit": { "type": "integer" }
            },
            "required": ["owner"]
        });
        let declaration = ts_declaration(
            "github.list_issues",
            Some("List issues"),
            Some(&schema),
            Some(&json!({ "type": "array", "items": { "type": "string" } })),
        );
        assert!(
            declaration.contains("declare function list_issues("),
            "{declaration}"
        );
        assert!(
            declaration.contains("/** Repository owner */ owner: string;"),
            "{declaration}"
        );
        assert!(declaration.contains("limit?: number;"), "{declaration}");
        assert!(
            declaration.contains("Promise<Array<string>>"),
            "{declaration}"
        );
    }

    #[test]
    fn const_and_enum_render_as_literals() {
        assert_eq!(schema_to_type(&json!({ "const": "start" })), "\"start\"");
        assert_eq!(
            schema_to_type(&json!({ "enum": ["a", "b"] })),
            "\"a\" | \"b\""
        );
    }

    #[test]
    fn over_long_schemas_fall_back_to_unknown() {
        let big = json!({
            "type": "object",
            "properties": {
                "field": { "type": "string", "description": "x".repeat(20_000) }
            }
        });
        assert_eq!(schema_to_type_bounded(&big, 1_000), "unknown");
    }
}
