//! The script result contract and its model-facing rendering.
//!
//! Everything here is engine-agnostic: a [`ScriptResult`] is what any
//! codemode sandbox produces, and [`render_result_text`] formats it the way
//! pi's `codemode` presents results to the model. This module (with
//! [`crate::signature`] and [`crate::identity`]) is the part of the crate a
//! second script backend could adopt without touching QuickJS.

use serde_json::Value;

/// One output item from a script.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::exhaustive_enums)] // plain data enum
pub enum OutputItem {
    /// A text item from `text()`.
    Text(String),
    /// A console line (`console.log` and friends).
    Console(String),
    /// An image from `image()`: raw base64 payload and its MIME type.
    Image {
        /// The base64-encoded image bytes.
        data: String,
        /// The image's MIME type, e.g. `image/png`.
        mime_type: String,
    },
}
/// One tool call recorded during execution.
#[derive(Debug, Clone)]
#[allow(clippy::exhaustive_structs)] // plain data record
pub struct ToolCall {
    /// The tool's registry path, as the script named it.
    pub name: String,
    /// Wall time of the upstream call in milliseconds.
    pub duration_ms: u128,
}
/// The script's result.
#[derive(Debug, Default)]
#[allow(clippy::exhaustive_structs)] // plain data record
pub struct ScriptResult {
    /// Whether the script completed without throwing.
    pub ok: bool,
    /// The script's error message when [`ScriptResult::ok`] is false: a
    /// thrown error, a rejected tool call, or a parse failure.
    pub error: Option<String>,
    /// The script's return value (JSON), when the script completed.
    pub value: Option<Value>,
    /// Output items, in the order the script produced them.
    pub output: Vec<OutputItem>,
    /// Tool calls made by the script, in order.
    pub calls: Vec<ToolCall>,
    /// Wall time of the whole script, in milliseconds.
    pub duration_ms: u128,
    /// The output budget in model tokens from the script's `@options` line
    /// (default [`DEFAULT_MAX_OUTPUT_TOKENS`]); applied by
    /// [`render_result_text`].
    pub max_output_tokens: u64,
}

/// Default output budget in model tokens (pi's default).
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 10_000;

/// Hard cap on total output characters across `text()` and `console` calls
/// (pi's cap). A script that pushes past it fails.
pub const MAX_OUTPUT_CHARS: usize = 16_777_216;

/// Hard cap on `text()` and `console` calls (pi's cap).
pub const MAX_OUTPUT_CALLS: usize = 100_000;

/// Characters per estimated model token, used to apply
/// [`ScriptResult::max_output_tokens`] to rendered text.
pub const CHARS_PER_TOKEN: usize = 4;

/// Parsed `@options` line from the first line of a script (pi's semantics:
/// the options line is optional, must be the first line, and is validated
/// before the script runs).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::exhaustive_structs)] // plain data record
pub struct ScriptOptions {
    /// Output budget in model tokens.
    pub max_output_tokens: Option<u64>,
    /// Whole-script deadline in milliseconds; overrides the host's default.
    pub timeout_ms: Option<u64>,
}

/// Total characters of an output list, for the output caps.
pub fn len_of(item: &OutputItem) -> usize {
    match item {
        OutputItem::Text(text) | OutputItem::Console(text) => text.len(),
        OutputItem::Image { data, .. } => data.len(),
    }
}
/// Parse an `image()` argument: a `data:<mime>;base64,<data>` URI, or a bare
/// base64 payload (assumed PNG).
pub fn parse_image_input(value: &str) -> Option<(String, String)> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("data:") {
        let (metadata, data) = rest.split_once(",")?;
        let mime_type = metadata
            .strip_suffix(";base64")
            .filter(|m| !m.is_empty())?
            .to_owned();
        if data.is_empty() {
            return None;
        }
        Some((mime_type, data.to_owned()))
    } else if !value.is_empty() {
        Some(("image/png".to_owned(), value.to_owned()))
    } else {
        None
    }
}
/// Render a [`ScriptResult`] as the text a host hands back to the model, in
/// pi's codemode format: a `Script completed`/`Script failed` header with the
/// wall time, text items (from `text()` and the return value) in order under
/// `==> text N/M <==` markers when there are several, `console` lines in one
/// `<console_output>` block, and `Script error:` last for failures. Output is
/// truncated to the script's `max_output_tokens` budget, keeping its start
/// and end; overflow is dropped (see [`render_result_text_with_overflow`]).
pub fn render_result_text(result: &ScriptResult) -> String {
    truncate_to_budget(render_sections(result), result.max_output_tokens)
}

/// Render a [`ScriptResult`] for the model, truncating at the script's
/// `max_output_tokens` budget. When the full output exceeds the budget, it is
/// written to `<overflow_dir>/codemode-output-<pid>-<nanos>.txt` and its path
/// is included in the result (pi: "longer output keeps its start and end, and
/// the full text is written to a temp file whose path is included in the
/// result"). A host without a writable directory can use
/// [`render_result_text`], which drops the overflow instead.
pub fn render_result_text_with_overflow(
    result: &ScriptResult,
    overflow_dir: &std::path::Path,
) -> String {
    let full = render_sections(result);
    let budget = result
        .max_output_tokens
        .saturating_mul(CHARS_PER_TOKEN as u64) as usize;
    if full.len() <= budget {
        return full;
    }
    let truncated = truncate_to_budget(full.clone(), result.max_output_tokens);
    let path = overflow_dir.join(format!(
        "codemode-output-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    match std::fs::write(&path, &full) {
        Ok(()) => format!("{truncated}\nFull output: {}", path.display()),
        Err(_) => truncated, // no writable directory: drop the overflow
    }
}

/// The untruncated result rendering shared by [`render_result_text`] and
/// [`render_result_text_with_overflow`].
pub fn render_sections(result: &ScriptResult) -> String {
    let mut sections: Vec<String> = Vec::new();
    // Text and image items appear in output order (pi); the return value
    // joins as a final text item (pi: `return value` adds the value like
    // `text()`).
    let mut items: Vec<OutputItem> = result
        .output
        .iter()
        .filter(|item| !matches!(item, OutputItem::Console(_)))
        .cloned()
        .collect();
    if let Some(value) = &result.value {
        items.push(OutputItem::Text(
            serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned()),
        ));
    }
    let text_count = items
        .iter()
        .filter(|item| matches!(item, OutputItem::Text(_)))
        .count();
    let image_count = items
        .iter()
        .filter(|item| matches!(item, OutputItem::Image { .. }))
        .count();
    let (mut text_index, mut image_index) = (0usize, 0usize);
    for item in &items {
        match item {
            OutputItem::Text(text) => {
                text_index += 1;
                if text_count > 1 {
                    sections.push(format!("==> text {text_index}/{text_count} <=="));
                }
                sections.push(text.clone());
            }
            OutputItem::Image { data, mime_type } => {
                image_index += 1;
                if image_count > 1 {
                    sections.push(format!("==> image {image_index}/{image_count} <=="));
                }
                sections.push(format!(
                    "[image: {mime_type}, {} bytes of base64]",
                    data.len()
                ));
            }
            OutputItem::Console(_) => unreachable!("filtered out above"),
        }
    }
    let console_lines: Vec<&String> = result
        .output
        .iter()
        .filter_map(|item| match item {
            OutputItem::Console(line) => Some(line),
            OutputItem::Text(_) | OutputItem::Image { .. } => None,
        })
        .collect();
    if !console_lines.is_empty() {
        sections.push("<console_output>".to_owned());
        sections.extend(console_lines.iter().map(|line| (*line).clone()));
    }
    if let Some(error) = &result.error {
        sections.push(format!("Script error: {error}"));
    }
    let status = if result.ok { "completed" } else { "failed" };
    format!(
        "Script {status} in {}ms\n{}",
        result.duration_ms,
        sections.join("\n")
    )
}

/// Apply the `max_output_tokens` budget (estimated at 4 characters per
/// token): over-budget output keeps its start and end around a marker.
pub fn truncate_to_budget(rendered: String, max_output_tokens: u64) -> String {
    let budget = max_output_tokens.saturating_mul(CHARS_PER_TOKEN as u64) as usize;
    if rendered.len() <= budget {
        return rendered;
    }
    let marker = format!("\n... [output truncated: kept start and end; budget {budget} chars]\n");
    if budget <= marker.len() + 64 {
        return rendered.chars().take(budget).collect();
    }
    let keep = budget - marker.len();
    let head = keep - keep / 2;
    let tail = keep / 2;
    let head_text: String = rendered.chars().take(head).collect();
    let total_chars = rendered.chars().count();
    let tail_text: String = rendered.chars().skip(total_chars - tail).collect();
    format!("{head_text}{marker}{tail_text}")
}
