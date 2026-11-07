//! Per-server exposure metadata.
//!
//! How a server's tools should be exposed to the model. The three variants
//! mirror the core of Codex's `ToolExposure` enum (which additionally has
//! model-only and hidden surfaces) so the convention reads the same across
//! hosts.

use std::fmt;

/// How a tool (or an entire upstream server) should be exposed.
///
/// - [`ToolExposure::Direct`] keeps today's behavior: the tool appears in
///   `tools/list` every turn with its full schema.
/// - [`ToolExposure::Deferred`] keeps the tool out of the model's tool list but
///   discoverable through a search primitive, with its schema fetched on demand
///   (Anthropic's [Tool Search Tool](https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-search-tool),
///   Zed discussion [#64004](https://github.com/zed-industries/zed/discussions/64004)).
/// - [`ToolExposure::CodeModeOnly`] makes the tool callable only from a script
///   sandbox; it never appears in the model's tool list, and results pipe
///   between tools without round-tripping through the model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[allow(clippy::exhaustive_enums)] // three fixed variants, mirrored across hosts
pub enum ToolExposure {
    /// In `tools/list` every turn (current behavior).
    #[default]
    Direct,
    /// Discoverable via search; schema fetched on demand.
    Deferred,
    /// Callable only from a script sandbox.
    CodeModeOnly,
}

impl ToolExposure {
    /// The lowercase name used in configuration files and logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Deferred => "deferred",
            Self::CodeModeOnly => "codemode",
        }
    }

    /// Whether the tool appears in the model-facing `tools/list` result.
    pub const fn is_listed(self) -> bool {
        matches!(self, Self::Direct)
    }

    /// Whether the tool is hidden from the model and reachable only through the
    /// script sandbox (or discovery primitives).
    pub const fn is_sandbox_only(self) -> bool {
        matches!(self, Self::CodeModeOnly)
    }
}

impl fmt::Display for ToolExposure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_is_listed_and_codemode_is_not() {
        assert!(ToolExposure::Direct.is_listed());
        assert!(!ToolExposure::Deferred.is_listed());
        assert!(!ToolExposure::CodeModeOnly.is_listed());
    }

    #[test]
    fn display_matches_config_names() {
        assert_eq!(ToolExposure::Direct.to_string(), "direct");
        assert_eq!(ToolExposure::Deferred.to_string(), "deferred");
        assert_eq!(ToolExposure::CodeModeOnly.to_string(), "codemode");
    }
}
