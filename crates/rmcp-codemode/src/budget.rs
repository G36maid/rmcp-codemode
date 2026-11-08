//! Resource budgets for one script run.
//!
//! pi's limits (defaults included) with OpenCode's call cap, collected into
//! one type instead of scattered fields. The script can override the output
//! budget and the deadline with its `// @options:` first line; everything
//! else is the host's voice.

use serde::{Deserialize, Serialize};

/// Resource budgets for one script run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
#[non_exhaustive]
pub struct Budget {
    /// Whole-run deadline in milliseconds, tool calls included. The script's
    /// `@options.timeout_ms` overrides it. `None` means no deadline (pi's
    /// default is 300000; the host decides here).
    pub timeout_ms: Option<u64>,
    /// Output budget in estimated model tokens (4 chars per token). The
    /// script's `@options.max_output_tokens` overrides it. Default: 10,000.
    pub max_output_tokens: u64,
    /// Hard cap on `text()`/`image()`/`console` items. Default: 100,000.
    pub max_output_items: usize,
    /// Hard cap on output characters (text and base64 image data).
    /// Default: 16,777,216 (16 MiB, pi's cap).
    pub max_output_chars: usize,
    /// Hard cap on tool calls one script may make. Default: 500.
    /// `None` means unlimited (OpenCode: `maxToolCalls`, absent = unlimited).
    pub max_tool_calls: Option<usize>,
    /// VM heap limit in bytes. Default: 256 MiB (pi's cap).
    pub memory_limit: usize,
    /// How many runs may execute at once; further runs queue in arrival
    /// order and fail without running if no slot frees within
    /// [`Budget::worker_wait_ms`] (pi: at most two codemode workers per
    /// process, 60 s wait). Default: 2.
    pub max_workers: usize,
    /// How long a run waits for a free worker slot, in milliseconds.
    /// Default: 60,000. The effective cap is the smaller of this and the
    /// run's deadline.
    pub worker_wait_ms: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            timeout_ms: None,
            max_output_tokens: 10_000,
            max_output_items: 100_000,
            max_output_chars: 16 * 1024 * 1024,
            max_tool_calls: Some(500),
            memory_limit: 256 * 1024 * 1024,
            max_workers: 2,
            worker_wait_ms: 60_000,
        }
    }
}

impl Budget {
    /// Characters per estimated model token, shared by the catalog's inline
    /// budget and the output truncation.
    pub const CHARS_PER_TOKEN: usize = 4;

    /// The default budget with one field overridden.
    pub fn with_max_output_tokens(mut self, tokens: u64) -> Self {
        self.max_output_tokens = tokens;
        self
    }

    /// The default budget with one field overridden.
    pub fn with_timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }

    /// The default budget with one field overridden.
    pub fn with_max_workers(mut self, workers: usize) -> Self {
        self.max_workers = workers;
        self
    }

    /// The default budget with one field overridden.
    pub fn with_worker_wait_ms(mut self, ms: u64) -> Self {
        self.worker_wait_ms = ms;
        self
    }

    /// The default budget with one field overridden.
    pub fn with_max_tool_calls(mut self, calls: usize) -> Self {
        self.max_tool_calls = Some(calls);
        self
    }

    /// The output budget in characters.
    pub fn output_char_budget(&self) -> usize {
        self.max_output_tokens
            .saturating_mul(Self::CHARS_PER_TOKEN as u64) as usize
    }
}

/// Maximum characters of JSON for one stored value (pi's cap).
pub const STORE_MAX_VALUE_CHARS: usize = 262_144;

/// Maximum characters of JSON across all stored values (pi's cap).
pub const STORE_MAX_TOTAL_CHARS: usize = 1_048_576;

/// The script's `// @options:` first line. Only these two fields are the
/// script's voice (pi's split); a malformed line fails the run before the
/// script executes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ScriptOptions {
    /// Overrides [`Budget::max_output_tokens`] for this run.
    pub max_output_tokens: Option<u64>,
    /// Overrides [`Budget::timeout_ms`] for this run.
    pub timeout_ms: Option<u64>,
}

/// Split off and validate a leading `// @options: {...}` line (pi's
/// semantics: optional, must be the first line, unknown or malformed fields
/// fail before the script runs). Returns the source unchanged and the parse
/// outcome.
pub fn extract_options(source: &str) -> (String, Result<ScriptOptions, String>) {
    let mut options = ScriptOptions::default();
    let mut lines = source.lines();
    let first_is_options = lines
        .next()
        .and_then(|first| first.trim().strip_prefix("// @options:"))
        .is_some();
    let mut parsed = Ok(options);
    if let Some(rest) = source
        .lines()
        .next()
        .and_then(|first| first.trim().strip_prefix("// @options:"))
    {
        let value: serde_json::Value = match serde_json::from_str(rest.trim()) {
            Ok(value) => value,
            Err(error) => {
                return (
                    source.to_owned(),
                    Err(format!("invalid @options line: {error}")),
                );
            }
        };
        let Some(object) = value.as_object() else {
            return (
                source.to_owned(),
                Err("invalid @options line: expected a JSON object".to_owned()),
            );
        };
        for (key, field) in object {
            match key.as_str() {
                "max_output_tokens" => match field.as_u64() {
                    Some(tokens) => options.max_output_tokens = Some(tokens),
                    None => {
                        return (
                            source.to_owned(),
                            Err("invalid @options line: max_output_tokens must be a non-negative integer".to_owned()),
                        );
                    }
                },
                "timeout_ms" => {
                    match field.as_u64() {
                        Some(ms) => options.timeout_ms = Some(ms),
                        None => {
                            return (
                            source.to_owned(),
                            Err("invalid @options line: timeout_ms must be a non-negative integer".to_owned()),
                        );
                        }
                    }
                }
                other => {
                    return (
                        source.to_owned(),
                        Err(format!("invalid @options line: unknown option '{other}'")),
                    );
                }
            }
        }
        parsed = Ok(options);
    }
    if !first_is_options && source.lines().skip(1).any(|l| l.contains("// @options:")) {
        return (
            source.to_owned(),
            Err("the @options line must be the first line".to_owned()),
        );
    }
    (source.to_owned(), parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_leading_options_line() {
        let (source, options) = extract_options(
            "// @options: {\"max_output_tokens\": 2000, \"timeout_ms\": 60000}\nreturn 1;",
        );
        assert_eq!(source.lines().count(), 2);
        assert_eq!(
            options.unwrap(),
            ScriptOptions {
                max_output_tokens: Some(2000),
                timeout_ms: Some(60000)
            }
        );
    }

    #[test]
    fn rejects_misplaced_and_malformed_lines() {
        for source in [
            "// @options: {oops}\nreturn 1;",
            "// @options: {\"nope\": 1}\nreturn 1;",
            "return 1;\n// @options: {\"timeout_ms\": 5}",
            "// @options: \"text\"\nreturn 1;",
        ] {
            let (_, result) = extract_options(source);
            assert!(result.is_err(), "{source}");
        }
    }

    #[test]
    fn plain_source_has_no_options() {
        let (_, options) = extract_options("return 1;");
        assert_eq!(options.unwrap(), ScriptOptions::default());
    }
}
