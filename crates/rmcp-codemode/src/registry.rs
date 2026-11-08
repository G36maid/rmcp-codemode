//! The aggregated tool registry.
//!
//! One registry holds every tool from every connected upstream server under a
//! namespaced path (`<namespace>.<tool_name>`), with per-tool exposure. This is
//! the "aggregated registry" from the aggregation interface: hosts read
//! [`Registry::listed_tools`] to build their `tools/list` result and
//! [`Registry::searchable`] to power discovery of deferred tools.

use rmcp::model::Tool;

use crate::{exposure::ToolExposure, naming::NamingConvention, search::SearchIndex};

/// A single tool in the registry.
#[derive(Clone, Debug)]
#[allow(clippy::exhaustive_structs)] // plain data record
pub struct RegistryEntry {
    /// The tool's full registry path, e.g. `github.list_issues`.
    pub path: String,
    /// The upstream server's namespace, e.g. `github`.
    pub namespace: String,
    /// The tool's original name on the upstream server.
    pub upstream_name: String,
    /// The upstream `tools/list` entry.
    pub tool: Tool,
    /// How this tool is exposed. Defaults to the upstream server's exposure.
    pub exposure: ToolExposure,
}

/// Metadata for a namespace: a group of related tools, such as the tools of
/// one MCP server (pi's `ToolNamespace`). Descriptions and instructions are
/// model-facing; aliases let scripts look the namespace up by a friendlier
/// name (pi: `describeNamespace("docs")` finds `mcp__docs`).
#[derive(Clone, Debug, Default)]
#[allow(clippy::exhaustive_structs)] // plain data record
pub struct NamespaceInfo {
    /// Shown once with the group in discovery results.
    pub description: Option<String>,
    /// Longer usage guidance, such as MCP server instructions. Returned by
    /// `describeNamespace` but not part of tool listings.
    pub instructions: Option<String>,
    /// Other names that resolve to this namespace in discovery lookups.
    pub aliases: Vec<String>,
}

/// An aggregated, namespaced tool registry.
///
/// Keys are full tool paths; iteration is in sorted path order, so `tools/list`
/// output is deterministic across restarts.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    entries: std::collections::BTreeMap<String, RegistryEntry>,
    /// Model-facing metadata per namespace (description, instructions,
    /// aliases) — pi's `ToolNamespace`.
    namespaces: std::collections::BTreeMap<String, NamespaceInfo>,
}

/// Error returned when a registry insertion collides.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// Two upstream servers declare the same namespace, or a tool path is
    /// already taken. The message names the conflicting path.
    #[error("duplicate tool path `{0}`: adjust the namespace or enable tool renaming")]
    DuplicatePath(String),
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a tool. `namespace` and `tool.name` form the registry path.
    pub fn insert(
        &mut self,
        namespace: &str,
        tool: Tool,
        exposure: ToolExposure,
    ) -> Result<&RegistryEntry, RegistryError> {
        self.insert_with_naming(namespace, &NamingConvention::Namespaced, tool, exposure)
    }

    /// Insert a tool under a path computed by `naming`. The entry still
    /// records the declared `namespace` (for descriptions and grouping);
    /// the path is whatever the convention spells — a flat path (no `.`)
    /// presents as a single segment in the sandbox tree.
    pub fn insert_with_naming(
        &mut self,
        namespace: &str,
        naming: &NamingConvention,
        tool: Tool,
        exposure: ToolExposure,
    ) -> Result<&RegistryEntry, RegistryError> {
        let path = naming.path(namespace, tool.name.as_ref());
        if self.entries.contains_key(&path) {
            return Err(RegistryError::DuplicatePath(path));
        }
        let entry = RegistryEntry {
            path: path.clone(),
            namespace: namespace.to_owned(),
            upstream_name: tool.name.to_string(),
            tool,
            exposure,
        };
        self.entries.insert(path.clone(), entry);
        Ok(&self.entries[&path])
    }

    /// Set a human-readable description for a namespace, shown in discovery.
    pub fn set_namespace_description(&mut self, namespace: &str, description: impl Into<String>) {
        self.namespaces
            .entry(namespace.to_owned())
            .or_default()
            .description = Some(description.into());
    }

    /// A namespace's description, if one was set.
    pub fn namespace_description(&self, namespace: &str) -> Option<&str> {
        self.namespaces
            .get(namespace)
            .and_then(|info| info.description.as_deref())
    }

    /// Set a namespace's longer usage guidance (pi's `instructions`, such as
    /// the MCP server's own instructions). Returned by `describeNamespace`
    /// but not part of tool listings.
    pub fn set_namespace_instructions(&mut self, namespace: &str, instructions: impl Into<String>) {
        self.namespaces
            .entry(namespace.to_owned())
            .or_default()
            .instructions = Some(instructions.into());
    }

    /// A namespace's usage guidance, if one was set.
    pub fn namespace_instructions(&self, namespace: &str) -> Option<&str> {
        self.namespaces
            .get(namespace)
            .and_then(|info| info.instructions.as_deref())
    }

    /// Add other names that resolve to this namespace in discovery lookups
    /// (pi: `describeNamespace("docs")` finds `mcp__docs`). The namespace's
    /// own name always matches too.
    pub fn add_namespace_aliases(&mut self, namespace: &str, aliases: &[String]) {
        self.namespaces
            .entry(namespace.to_owned())
            .or_default()
            .aliases
            .extend(aliases.iter().cloned());
    }

    /// Resolve a script-supplied namespace name to a canonical namespace:
    /// the name itself, then any namespace whose own name or one of its
    /// aliases matches (pi's `isNamespaceName`: written as-is, or as the
    /// same script identifier, so `dev-radius` and `dev_radius` both hit).
    pub fn resolve_namespace(&self, name: &str) -> Option<String> {
        if self.namespaces.contains_key(name) {
            return Some(name.to_owned());
        }
        let normalized = crate::identity::normalize_identifier(name);
        self.namespaces
            .iter()
            .find(|(canonical, info)| {
                crate::identity::normalize_identifier(canonical) == normalized
                    || info
                        .aliases
                        .iter()
                        .any(|alias| crate::identity::normalize_identifier(alias) == normalized)
            })
            .map(|(canonical, _)| canonical.clone())
    }

    /// A namespace's metadata, if any was set.
    pub fn namespace_info(&self, namespace: &str) -> Option<&NamespaceInfo> {
        self.namespaces.get(namespace)
    }

    /// All entries, in sorted path order.
    pub fn entries(&self) -> impl Iterator<Item = &RegistryEntry> {
        self.entries.values()
    }

    /// Look up one tool by full registry path.
    pub fn get(&self, path: &str) -> Option<&RegistryEntry> {
        self.entries.get(path)
    }

    /// Change one tool's exposure in place. Returns whether the path existed.
    pub fn override_exposure(&mut self, path: &str, exposure: ToolExposure) -> bool {
        if let Some(entry) = self.entries.get_mut(path) {
            entry.exposure = exposure;
            true
        } else {
            false
        }
    }

    /// Number of tools in the registry.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Tools that appear in the model-facing `tools/list` result.
    pub fn listed_tools(&self) -> impl Iterator<Item = &RegistryEntry> {
        self.entries.values().filter(|e| e.exposure.is_listed())
    }

    /// Tools hidden from `tools/list` but discoverable via search.
    pub fn deferred_tools(&self) -> impl Iterator<Item = &RegistryEntry> {
        self.entries
            .values()
            .filter(|e| matches!(e.exposure, ToolExposure::Deferred))
    }

    /// Build a search index over every tool, with namespace descriptions and
    /// input property names as extra searchable text.
    ///
    /// This is the discovery primitive behind deferred and codemode tools: a
    /// host (or the script sandbox's `searchTools()`) ranks the full registry
    /// against a query, then fetches schemas on demand via [`Registry::get`].
    pub fn search_index(&self) -> SearchIndex {
        let extra = |entry: &RegistryEntry| -> Vec<String> {
            let mut texts: Vec<String> = Vec::new();
            if let Some(info) = self.namespaces.get(&entry.namespace) {
                if let Some(desc) = &info.description {
                    texts.push(desc.clone());
                }
                if let Some(instructions) = &info.instructions {
                    texts.push(instructions.clone());
                }
            }
            if let Some(props) = entry
                .tool
                .input_schema
                .get("properties")
                .and_then(|p| p.as_object())
            {
                for (name, prop) in props {
                    texts.push(name.clone());
                    if let Some(d) = prop.get("description").and_then(|d| d.as_str()) {
                        texts.push(d.to_owned());
                    }
                }
            }
            texts
        };
        SearchIndex::build(self.entries.values().map(|e| {
            (
                e.path.clone(),
                e.tool.description.clone().unwrap_or_default().to_string(),
                extra(e),
            )
        }))
    }
}

#[cfg(test)]
mod tests {

    use rmcp::model::Tool;
    use serde_json::{Value, json};

    use super::*;
    use crate::exposure::ToolExposure;

    fn tool(name: &str, description: &str, props: Value) -> Tool {
        let schema = props.as_object().cloned().unwrap_or_default();
        Tool::new(name.to_owned(), description.to_owned(), schema)
    }

    #[test]
    fn tools_are_namespaced() {
        let mut registry = Registry::new();
        registry
            .insert(
                "github",
                tool("list_issues", "List issues", json!({})),
                ToolExposure::Direct,
            )
            .unwrap();
        registry
            .insert(
                "github",
                tool("search_code", "Search code", json!({})),
                ToolExposure::Deferred,
            )
            .unwrap();
        registry
            .insert(
                "ghidra",
                tool("decompile", "Decompile", json!({})),
                ToolExposure::CodeModeOnly,
            )
            .unwrap();

        assert_eq!(registry.len(), 3);
        let paths: Vec<_> = registry.entries().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "ghidra.decompile",
                "github.list_issues",
                "github.search_code"
            ]
        );
    }

    #[test]
    fn duplicate_paths_are_rejected() {
        let mut registry = Registry::new();
        registry
            .insert(
                "github",
                tool("get_me", "a", json!({})),
                ToolExposure::Direct,
            )
            .unwrap();
        let err = registry
            .insert(
                "github",
                tool("get_me", "b", json!({})),
                ToolExposure::Direct,
            )
            .unwrap_err();
        assert!(matches!(err, RegistryError::DuplicatePath(p) if p == "github.get_me"));
    }

    #[test]
    fn exposure_partitions() {
        let mut registry = Registry::new();
        registry
            .insert("a", tool("direct", "d", json!({})), ToolExposure::Direct)
            .unwrap();
        registry
            .insert(
                "a",
                tool("deferred", "d", json!({})),
                ToolExposure::Deferred,
            )
            .unwrap();
        registry
            .insert("a", tool("cm", "d", json!({})), ToolExposure::CodeModeOnly)
            .unwrap();

        let listed: Vec<_> = registry.listed_tools().map(|e| e.path.as_str()).collect();
        assert_eq!(listed, ["a.direct"]);
        let deferred: Vec<_> = registry.deferred_tools().map(|e| e.path.as_str()).collect();
        assert_eq!(deferred, ["a.deferred"]);
    }

    #[test]
    fn search_index_includes_property_names() {
        let mut registry = Registry::new();
        registry.set_namespace_description("ghidra", "Reverse engineering with Ghidra");
        registry.insert(
            "ghidra",
            tool(
                "list_functions",
                "List functions",
                json!({"type": "object", "properties": {"filter": {"type": "string", "description": "name filter"}}}),
            ),
            ToolExposure::CodeModeOnly,
        )
        .unwrap();

        let index = registry.search_index();
        let hits = index.search("function filter", 5);
        assert_eq!(hits[0].name, "ghidra.list_functions");

        // Namespace description text is searchable too.
        let hits = index.search("reverse engineering", 5);
        assert_eq!(hits[0].name, "ghidra.list_functions");
    }

    #[test]
    fn get_by_path() {
        let mut registry = Registry::new();
        registry
            .insert(
                "fs",
                tool("read", "Read", json!({})),
                ToolExposure::Deferred,
            )
            .unwrap();
        assert!(registry.get("fs.read").is_some());
        assert!(registry.get("fs.write").is_none());
    }
}
