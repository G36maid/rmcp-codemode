//! TypeScript declaration rendering.
//!
//! Renders a tool's JSON Schema as the TypeScript declaration the model sees
//! in `describeTool`, `describeNamespace`, and the inline `codemode`
//! description. Engine-agnostic: operates on plain JSON values.

use serde_json::Value;

use crate::identity::normalize_identifier;

/// Render a tool's TypeScript declaration (pi's `describeTool` shape): a doc
/// comment with the description, and a function taking one typed `args`
/// object. Without a schema the argument type is an open record.
pub fn ts_declaration(name: &str, description: Option<&str>, schema: Option<&Value>) -> String {
    let callable = name.rsplit('.').next().unwrap_or(name);
    let callable = normalize_identifier(callable);
    let args_type = match schema {
        Some(schema) if schema.get("type").and_then(Value::as_str) == Some("object") => {
            ts_object_type(schema)
        }
        _ => "{ [k: string]: unknown }".to_owned(),
    };
    match description {
        Some(description) if !description.is_empty() => {
            format!(
                "/** {description} */\ndeclare function {callable}(args: {args_type}): Promise<unknown>;"
            )
        }
        _ => format!("declare function {callable}(args: {args_type}): Promise<unknown>;"),
    }
}

/// Render a JSON Schema object type as a TypeScript object type.
pub(crate) fn ts_object_type(schema: &Value) -> String {
    let properties = match schema.get("properties").and_then(Value::as_object) {
        Some(properties) if !properties.is_empty() => properties,
        _ => return "{ [k: string]: unknown }".to_owned(),
    };
    let members = ts_members(properties, schema);
    if members.is_empty() {
        "{ [k: string]: unknown }".to_owned()
    } else {
        format!("{{ {} }}", members.join(" "))
    }
}

/// Render the members of an object schema: `name: type;` with `?` markers
/// for properties absent from `required`, and doc comments when present.
pub(crate) fn ts_members(
    properties: &serde_json::Map<String, Value>,
    schema: &Value,
) -> Vec<String> {
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
        let member_type = ts_type(property_schema);
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
    members
}

/// Render one JSON Schema value as a TypeScript type.
pub(crate) fn ts_type(schema: &Value) -> String {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        let literals: Vec<String> = values
            .iter()
            .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "unknown".to_owned()))
            .collect();
        if !literals.is_empty() {
            return literals.join(" | ");
        }
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => "string".to_owned(),
        Some("number" | "integer") => "number".to_owned(),
        Some("boolean") => "boolean".to_owned(),
        Some("null") => "null".to_owned(),
        Some("array") => format!(
            "{}[]",
            schema
                .get("items")
                .map(ts_type)
                .unwrap_or_else(|| "unknown".to_owned())
        ),
        Some("object") => match schema.get("properties").and_then(Value::as_object) {
            Some(properties) if !properties.is_empty() => ts_object_type(schema),
            _ => "Record<string, unknown>".to_owned(),
        },
        _ => "unknown".to_owned(),
    }
}
