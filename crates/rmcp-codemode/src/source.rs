//! Codemode source format: JavaScript, optionally preceded by one options
//! line. A line-for-line Rust mirror of pi's
//! `packages/codemode/src/source.ts`. The host parses the model's tool input
//! before handing the script to an engine; the options line is replaced by an
//! empty line, so line numbers are unchanged.
//!
//! ```js
//! // @options: {"max_output_tokens": 2000, "timeout_ms": 30000}
//! const text = await tools.read({ path: "package.json" });
//! text(JSON.parse(text).name);
//! ```

use serde_json::Value;

/// (pi: `CODEMODE_OPTIONS_PREFIX`.)
pub const CODEMODE_OPTIONS_PREFIX: &str = "// @options:";

const SUPPORTED_FIELDS: [&str; 2] = ["max_output_tokens", "timeout_ms"];
const SUPPORTED_FIELDS_TEXT: &str = "`max_output_tokens` and `timeout_ms`";
/// Largest delay `setTimeout` supports, which bounds `timeout_ms`.
const MAX_TIMEOUT_MS: u64 = 2_147_483_647;

/// The options a source line may set. (pi: `CodemodeSourceOptions`.)
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourceOptions {
    /// Token budget for the script's output.
    pub max_output_tokens: Option<u64>,
    /// Hard deadline for the whole script in milliseconds, including tool calls.
    pub timeout_ms: Option<u64>,
}

/// A parsed source. (pi: `ParsedCodemodeSource`.)
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedSource {
    /// The script with the options line replaced by an empty line, so line numbers are unchanged.
    pub code: String,
    pub options: SourceOptions,
}

/// Invalid input: empty source, malformed options, or an options line that is
/// not the first line (which would otherwise be ignored without notice). (pi:
/// `CodemodeSourceError`.)
#[derive(Debug, Clone, PartialEq)]
pub struct CodemodeSourceError(pub String);

impl std::fmt::Display for CodemodeSourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CodemodeSourceError {}

fn parse_options(directive: &str) -> Result<SourceOptions, CodemodeSourceError> {
    if directive.is_empty() {
        return Err(CodemodeSourceError(format!(
            "@options must be a JSON object with supported fields {SUPPORTED_FIELDS_TEXT}"
        )));
    }
    let value: Value = serde_json::from_str(directive).map_err(|error| {
        CodemodeSourceError(format!(
            "@options must be valid JSON with supported fields {SUPPORTED_FIELDS_TEXT}: {error}"
        ))
    })?;
    let Value::Object(fields) = value else {
        return Err(CodemodeSourceError(format!(
            "@options must be a JSON object with supported fields {SUPPORTED_FIELDS_TEXT}"
        )));
    };
    for key in fields.keys() {
        if !SUPPORTED_FIELDS.contains(&key.as_str()) {
            return Err(CodemodeSourceError(format!(
                "@options only supports {SUPPORTED_FIELDS_TEXT}; got `{key}`"
            )));
        }
    }
    let mut options = SourceOptions::default();
    let (max_output_tokens, timeout_ms) =
        (fields.get("max_output_tokens"), fields.get("timeout_ms"));
    // pi: `typeof value === "number" && Number.isSafeInteger(value) && value >= 0` —
    // an integral f64 (JSON `1e3`) is a safe integer too.
    let safe_integer = |value: &Value| -> Option<u64> {
        let float = value.as_f64()?;
        if float.fract() != 0.0 || !(0.0..=9_007_199_254_740_991.0).contains(&float) {
            return None;
        }
        Some(float as u64)
    };
    if let Some(value) = max_output_tokens {
        let Some(tokens) = safe_integer(value) else {
            return Err(CodemodeSourceError(
                "@options field `max_output_tokens` must be a non-negative safe integer".into(),
            ));
        };
        options.max_output_tokens = Some(tokens);
    }
    if let Some(value) = timeout_ms {
        let Some(ms) = safe_integer(value).filter(|ms| *ms != 0 && *ms <= MAX_TIMEOUT_MS) else {
            return Err(CodemodeSourceError(format!(
                "@options field `timeout_ms` must be a positive integer up to {MAX_TIMEOUT_MS}"
            )));
        };
        options.timeout_ms = Some(ms);
    }
    Ok(options)
}

/// Split an optional first-line `// @options: {...}` from the script. Throws
/// [`CodemodeSourceError`] for empty input, invalid options, and an options line that is not the
/// first line, which would otherwise be ignored without notice. (pi:
/// `parseCodemodeSource`.)
pub fn parse_codemode_source(input: &str) -> Result<ParsedSource, CodemodeSourceError> {
    if input.trim().is_empty() {
        return Err(CodemodeSourceError(
            "Expected JavaScript source text (non-empty). Provide JS only, optionally with a first line `// @options: {\"max_output_tokens\": 1000}`.".into(),
        ));
    }
    let lines: Vec<&str> = input.split('\n').collect();
    let misplaced = lines.iter().enumerate().position(|(index, line)| {
        index > 0 && line.trim_start().starts_with(CODEMODE_OPTIONS_PREFIX)
    });
    if let Some(misplaced) = misplaced {
        return Err(CodemodeSourceError(format!(
            "The {CODEMODE_OPTIONS_PREFIX} line must be the first line of the input; found one on line {}",
            misplaced + 1
        )));
    }
    let Some(newline) = input.find('\n') else {
        // `lines[0]` always exists: `input` is non-empty.
        let first_line = lines[0];
        let trimmed = first_line.trim_start();
        if !trimmed.starts_with(CODEMODE_OPTIONS_PREFIX) {
            return Ok(ParsedSource {
                code: input.to_string(),
                options: SourceOptions::default(),
            });
        }
        return Err(CodemodeSourceError(
            "The @options line must be followed by JavaScript source on subsequent lines".into(),
        ));
    };
    let first_line = lines[0].strip_suffix('\r').unwrap_or(lines[0]);
    let trimmed = first_line.trim_start();
    if !trimmed.starts_with(CODEMODE_OPTIONS_PREFIX) {
        return Ok(ParsedSource {
            code: input.to_string(),
            options: SourceOptions::default(),
        });
    }
    let code = input[newline..].to_string();
    if code.trim().is_empty() {
        return Err(CodemodeSourceError(
            "The @options line must be followed by JavaScript source on subsequent lines".into(),
        ));
    }
    Ok(ParsedSource {
        code,
        options: parse_options(trimmed[CODEMODE_OPTIONS_PREFIX.len()..].trim())?,
    })
}
