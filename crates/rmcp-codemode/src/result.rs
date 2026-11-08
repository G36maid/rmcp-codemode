//! The script contract and its model-facing rendering.
//!
//! pi's result contract: what a run reports (output items, call records,
//! store writes), and the exact text a host hands back to the model —
//! `Script completed`/`Script failed` with wall time, `==> text N/M <==`
//! markers, one `<console_output>` block, `Script error:` on failure, and
//! start-and-end-preserving truncation with an overflow file.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::budget::Budget;

/// One output item, in the order the script produced it (pi's shape).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputItem {
    /// A text item from `text()` or the return value.
    Text {
        /// The text.
        text: String,
    },
    /// A console line (`console.log` and friends).
    Console {
        /// The formatted line.
        text: String,
    },
    /// An image from `image()`: base64 payload and detected MIME type.
    Image {
        /// The base64 payload.
        data: String,
        /// The detected MIME type.
        mime_type: String,
    },
}

/// One tool call the script made (pi's `CodemodeCall`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub struct CallRecord {
    /// The tool's canonical path, as the script named it (identifier form).
    pub name: String,
    /// `ok` on success, `error` on a rejected call, `cancelled` when the run
    /// ended before the call settled (pi's statuses).
    pub status: CallStatus,
    /// Wall time of the call in milliseconds.
    pub duration_ms: u64,
}

impl CallRecord {
    /// A finished call.
    pub fn finished(name: impl Into<String>, status: CallStatus, duration_ms: u64) -> Self {
        Self {
            name: name.into(),
            status,
            duration_ms,
        }
    }
}

/// How one call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CallStatus {
    /// The call resolved.
    Ok,
    /// The call rejected.
    Error,
    /// The run ended before the call settled (pi: cancelled, not undone).
    Cancelled,
}

/// Store writes a successful run committed (pi's `CodemodeStoreWrites`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub struct StoreWrites {
    /// Keys set by `store(key, value)`.
    pub set: Vec<(String, Value)>,
    /// Keys deleted with `store(key, undefined)`.
    pub delete: Vec<String>,
}

/// What a run reported. `output` survives failures up to the failure; a
/// successful run carries the store writes (pi's tagged result).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ScriptResult {
    /// Whether the script completed (returned, threw nothing, or `exit()`).
    pub ok: bool,
    /// The script's return value, on success.
    pub value: Option<Value>,
    /// The failure's error text, when `ok` is false.
    pub error: Option<String>,
    /// Output items, in production order.
    pub output: Vec<OutputItem>,
    /// Tool calls made, in order — including cancelled ones (pi: calls are
    /// real and not undone).
    pub calls: Vec<CallRecord>,
    /// Store writes, on success.
    pub store_writes: StoreWrites,
    /// Wall time of the whole run, in milliseconds.
    pub duration_ms: u64,
    /// The output budget that applied (the script's `@options` override or
    /// the [`Budget`] default).
    pub max_output_tokens: u64,
    /// The file holding the full output when truncation spilled it (pi).
    pub full_output_path: Option<String>,
}

impl ScriptResult {
    /// A failed run with only an error message (before the script ran).
    pub fn failed(message: impl Into<String>, max_output_tokens: u64, duration_ms: u64) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            max_output_tokens,
            duration_ms,
            ..Self::default()
        }
    }
}

/// Total characters of one item, for the output caps.
pub fn item_chars(item: &OutputItem) -> usize {
    match item {
        OutputItem::Text { text } | OutputItem::Console { text } => text.len(),
        OutputItem::Image { data, .. } => data.len(),
    }
}

/// The untruncated result rendering shared by [`render_result_text`] and
/// [`render_result_text_with_overflow`].
pub fn render_sections(result: &ScriptResult) -> String {
    let mut sections: Vec<String> = Vec::new();
    // Text and image items appear in output order (pi); the return value
    // joins as a final text item.
    let mut items: Vec<OutputItem> = result
        .output
        .iter()
        .filter(|item| !matches!(item, OutputItem::Console { .. }))
        .cloned()
        .collect();
    if let Some(value) = &result.value {
        items.push(OutputItem::Text {
            text: serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned()),
        });
    }
    let text_count = items
        .iter()
        .filter(|item| matches!(item, OutputItem::Text { .. }))
        .count();
    let image_count = items
        .iter()
        .filter(|item| matches!(item, OutputItem::Image { .. }))
        .count();
    let (mut text_index, mut image_index) = (0usize, 0usize);
    for item in &items {
        match item {
            OutputItem::Text { text } => {
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
            OutputItem::Console { .. } => unreachable!("filtered out above"),
        }
    }
    let console_lines: Vec<&String> = result
        .output
        .iter()
        .filter_map(|item| match item {
            OutputItem::Console { text } => Some(text),
            _ => None,
        })
        .collect();
    if !console_lines.is_empty() {
        sections.push("<console_output>".to_owned());
        sections.extend(console_lines.into_iter().cloned());
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

/// Apply the token budget: over-budget output keeps its start and end
/// around a marker (pi's shape, minus the spill file).
pub fn truncate_to_budget(rendered: String, max_output_tokens: u64) -> String {
    let budget = max_output_tokens.saturating_mul(Budget::CHARS_PER_TOKEN as u64) as usize;
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
    let total = rendered.chars().count();
    let tail_text: String = rendered.chars().skip(total - tail).collect();
    format!("{head_text}{marker}{tail_text}")
}

/// Render a [`ScriptResult`] as the text a host hands back to the model,
/// truncated at the output budget; overflow is dropped.
pub fn render_result_text(result: &ScriptResult) -> String {
    truncate_to_budget(render_sections(result), result.max_output_tokens)
}

/// Render with overflow spill: the full text is written under `overflow_dir`
/// and its path is included (pi: "the full text is written to a temp file
/// whose path is included in the result").
pub fn render_result_text_with_overflow(
    result: &ScriptResult,
    overflow_dir: &std::path::Path,
) -> String {
    let full = render_sections(result);
    let budget = result
        .max_output_tokens
        .saturating_mul(Budget::CHARS_PER_TOKEN as u64) as usize;
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
        Err(_) => truncated,
    }
}
