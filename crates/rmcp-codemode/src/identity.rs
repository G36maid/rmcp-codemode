//! Tool-path identity rules.
//!
//! How a registry path is spelled as a script identifier and how unknown
//! names get self-correction suggestions. Engine-agnostic.

/// Normalize a tool path into a valid JavaScript identifier (pi's
/// `toCodemodeIdentifier`): characters that are not valid in identifiers
/// become `_`, and a leading digit is prefixed so the result is a valid name.
/// Dots become `_`, so `github.list_issues` is also reachable as
/// `tools.github_list_issues`.
pub fn normalize_identifier(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 1);
    for ch in path.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// The error message for an unknown tool: a close-match suggestion when one
/// exists (pi: "Did you mean tools.web_search?"), otherwise an availability
/// list (pi: "Available: echo, web_search.").
pub fn unknown_tool_message(path: &str, names: &[String]) -> String {
    // Separator-insensitive identity first (pi: `websearch` suggests
    // `web-search`), then shared-token scoring.
    let squash = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
            .to_lowercase()
    };
    let requested_squashed = squash(path);
    if let Some(known) = names
        .iter()
        .find(|known| squash(known) == requested_squashed)
    {
        return format!("Unknown tool '{path}'. Did you mean '{known}'?");
    }
    let requested = tokenize_path(path);
    let mut best: Option<(&str, usize)> = None;
    for known in names {
        let known_tokens = tokenize_path(known);
        let shared = known_tokens
            .iter()
            .filter(|t| requested.contains(t))
            .count();
        let score = shared * 2;
        if score > 0 && best.is_none_or(|(_, best_score)| score > best_score) {
            best = Some((known, score));
        }
    }
    match best {
        Some((known, _)) => format!("Unknown tool '{path}'. Did you mean '{known}'?"),
        None => {
            let available: Vec<&str> = names.iter().take(3).map(String::as_str).collect();
            if available.is_empty() {
                format!("Unknown tool '{path}'. No tools are registered.")
            } else {
                format!(
                    "Unknown tool '{path}'. Available: {}.",
                    available.join(", ")
                )
            }
        }
    }
}
/// Split a tool path into lowercase word tokens: separators (`.`, `-`, `_`)
/// and camelCase boundaries both split, so `websearch` shares the token
/// `search` with `web-search` and `getZone` with `get_zone`.
/// Split a tool path into lowercase word tokens: separators (`.`, `-`, `_`)
/// and camelCase boundaries both split, so `websearch` shares the token
/// `search` with `web-search` and `getZone` with `get_zone`. Delegates to
/// [`crate::search::tokenize`] — one tokenizer serves BM25 and suggestions.
pub(crate) fn tokenize_path(path: &str) -> Vec<String> {
    crate::search::tokenize(path)
}
