//! Tool-path naming conventions.
//!
//! Different hosts and model vendors expect different tool spellings —
//! Claude Code flattens to `mcp__<server>__<tool>`, OpenCode nests real
//! namespaces (`github.create_issue`), Cloudflare flattens without any
//! server prefix. Because the sandbox tree is *built from* registry paths,
//! any of these spellings work end to end: a flat path is just a single-
//! segment leaf, a dotted path nests. The convention only decides how an
//! upstream's `(namespace, tool_name)` pair becomes a registry path;
//! everything downstream (routing, exposure, discovery, the sandbox proxy
//! tree) follows whatever paths exist.
//!
//! The default is [`NamingConvention::Namespaced`] — the hierarchical model
//! shared with OpenCode, and the only one that keeps namespace enumeration,
//! namespace-scoped search, and per-namespace inline declarations.

use std::sync::Arc;

/// How an upstream's `(namespace, tool_name)` pair becomes a registry path.
#[derive(Default)]
#[non_exhaustive] // new conventions may be added; use Custom for one-off spellings
pub enum NamingConvention {
    /// `{namespace}.{tool}` — hierarchical (OpenCode-style). The default.
    /// Preserves `Object.keys` namespace enumeration, namespace-scoped
    /// `searchTools`, and per-namespace inline declarations.
    #[default]
    Namespaced,
    /// `mcp__{namespace}__{tool}` — Claude Code's flat convention. The whole
    /// path is one segment: scripts call `tools.mcp__github__create_issue`
    /// and `ALL_TOOLS` lists it as such.
    McpPrefixed,
    /// `{namespace}{separator}{tool}` — any other flat spelling.
    Flat {
        /// Glued between the namespace and the tool name, e.g. `__` or `-`.
        separator: String,
    },
    /// A fully custom mapping. The returned path is used as-is; a path
    /// containing `.` nests in the sandbox tree, a flat one does not.
    /// Path collisions across tools still fail registration with
    /// [`crate::registry::RegistryError::DuplicatePath`].
    Custom(PathMapping),
}

/// A custom `(namespace, tool_name) -> path` mapping.
pub type PathMapping = Arc<dyn Fn(&str, &str) -> String + Send + Sync>;

impl NamingConvention {
    /// Compute the registry path for one upstream tool.
    pub fn path(&self, namespace: &str, tool_name: &str) -> String {
        match self {
            Self::Namespaced => format!("{namespace}.{tool_name}"),
            Self::McpPrefixed => format!("mcp__{namespace}__{tool_name}"),
            Self::Flat { separator } => format!("{namespace}{separator}{tool_name}"),
            Self::Custom(mapping) => mapping(namespace, tool_name),
        }
    }
}

impl std::fmt::Debug for NamingConvention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Namespaced => f.write_str("Namespaced"),
            Self::McpPrefixed => f.write_str("McpPrefixed"),
            Self::Flat { separator } => f
                .debug_struct("Flat")
                .field("separator", separator)
                .finish(),
            Self::Custom(_) => f.write_str("Custom"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaced_is_hierarchical() {
        assert_eq!(
            NamingConvention::Namespaced.path("github", "create_issue"),
            "github.create_issue"
        );
    }

    #[test]
    fn mcp_prefixed_matches_claude_code() {
        assert_eq!(
            NamingConvention::McpPrefixed.path("github", "create_issue"),
            "mcp__github__create_issue"
        );
    }

    #[test]
    fn flat_uses_the_separator() {
        let flat = NamingConvention::Flat {
            separator: "-".to_owned(),
        };
        assert_eq!(flat.path("weather", "get-temp"), "weather-get-temp");
    }

    #[test]
    fn custom_decides() {
        let custom = NamingConvention::Custom(Arc::new(|ns, tool| format!("{ns}/{tool}")));
        assert_eq!(custom.path("github", "create_issue"), "github/create_issue");
    }
}
