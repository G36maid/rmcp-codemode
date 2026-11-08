//! Tool-path identity rules: script identifiers, close-match suggestions.
//!
//! pi's rules, shared by every view: characters outside identifiers become
//! `_` (pi's `toCodemodeIdentifier`), and an unknown name fails with the
//! closest real names (pi's guard: separator-insensitive exact forms first,
//! then two-way substring matches, up to five suggestions, else a short
//! availability list, plus the `in`-check hint).

/// Normalize a tool path into a valid JavaScript identifier (pi's
/// `toCodemodeIdentifier`): characters that are not valid in identifiers
/// become `_`, and a leading digit is prefixed so the result is valid.
/// Dots become `_`, so `github.create_issue` also reaches as
/// `tools.github_create_issue`.
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

/// pi's guard text for a member that does not exist: close matches first
/// (separator-insensitive identity, then two-way substring containment, up
/// to five), else an availability list when the set is small, always ending
/// with the `in`-check hint that lets a script verify before calling.
pub fn unknown_member_message(label: &str, property: &str, names: &[String], hint: &str) -> String {
    let comparable = |name: &str| -> String {
        name.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_lowercase()
    };
    let wanted = comparable(property);
    let exact: Vec<&String> = names
        .iter()
        .filter(|name| comparable(name) == wanted)
        .collect();
    let close: Vec<&String> = if exact.is_empty() {
        names
            .iter()
            .filter(|name| {
                let candidate = comparable(name);
                !wanted.is_empty() && (candidate.contains(&wanted) || wanted.contains(&candidate))
            })
            .collect()
    } else {
        exact
    };
    let mut message = format!("{label}.{property} does not exist.");
    if !close.is_empty() {
        let suggestions: Vec<String> = close
            .iter()
            .take(5)
            .map(|name| format!("{label}.{name}"))
            .collect();
        message.push_str(&format!(" Did you mean {}?", suggestions.join(", ")));
    } else if names.len() <= 20 {
        message.push_str(&format!(" Available: {}.", names.join(", ")));
    }
    if !hint.is_empty() {
        message.push(' ');
        message.push_str(hint);
    }
    message.push_str(&format!(
        " Check for a member with \"{}\" in {label}.",
        property
    ));
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_like_pi() {
        assert_eq!(
            normalize_identifier("mcp__docs__search"),
            "mcp__docs__search"
        );
        assert_eq!(normalize_identifier("my-tool"), "my_tool");
        assert_eq!(
            normalize_identifier("github.create_issue"),
            "github_create_issue"
        );
        assert_eq!(normalize_identifier("9lives"), "_9lives");
    }

    #[test]
    fn guard_suggests_close_matches_like_pi() {
        let names: Vec<String> = ["web_search", "web_fetch"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let message = unknown_member_message("tools", "websearch", &names, "hint");
        assert!(
            message.contains("Did you mean tools.web_search?"),
            "{message}"
        );
        assert!(message.ends_with("Check for a member with \"websearch\" in tools."));
    }

    #[test]
    fn guard_lists_availability_for_small_sets() {
        let names: Vec<String> = vec!["echo".to_owned(), "read".to_owned()];
        let message = unknown_member_message("tools", "nothing", &names, "");
        assert!(message.contains("Available: echo, read."), "{message}");
    }
}
