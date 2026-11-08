//! The tool space: the single source of truth for tools and namespaces.
//!
//! One model absorbs the three designs' divergent shapes:
//!
//! - **OpenCode/CF (hierarchical)**: a path's segments *are* the namespaces.
//!   `["github", "create_issue"]` nests; `Object.keys` enumerates the tree;
//!   namespace metadata is attached to intermediate segments.
//! - **pi (flat + metadata)**: every tool is one flat identifier
//!   (`mcp__docs__search`), and namespaces exist as *metadata* attached to
//!   entries ([`NamespaceMeta`] with description, instructions, aliases) —
//!   `describeNamespace("docs")` resolves through an alias.
//!
//! The space stores segments and metadata; views over it decide the shape.
//! A flat spelling is simply a one-segment path with namespace metadata
//! attached, so pi's model is a projection of the same store.

use std::{collections::BTreeMap, sync::Arc};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How a tool is exposed to the model (the convention pi, Codex, and
/// OpenCode all share).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Exposure {
    /// In `tools/list` every turn (today's behavior).
    #[default]
    Direct,
    /// Discoverable via search; the schema is fetched on demand.
    Deferred,
    /// Callable only from a script sandbox; results pipe between tools.
    CodeModeOnly,
}

impl Exposure {
    /// Whether the tool appears in the model-facing `tools/list`.
    pub const fn is_listed(self) -> bool {
        matches!(self, Self::Direct)
    }
}

/// Namespace-level metadata (pi's `ToolNamespace`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
#[non_exhaustive]
pub struct NamespaceMeta {
    /// Shown once with the group in listings; participates in search for
    /// every descendant tool (OpenCode's rule).
    pub description: Option<String>,
    /// Longer usage guidance, such as the upstream MCP server's own
    /// instructions. Returned by `describeNamespace`, never in listings.
    pub instructions: Option<String>,
    /// Other names that resolve to this namespace (pi: `describeNamespace(
    /// "docs")` finds `mcp__docs`). Matched as written or as the same script
    /// identifier.
    pub aliases: Vec<String>,
}

impl NamespaceMeta {
    /// Metadata with only a description.
    pub fn with_description(description: impl Into<String>) -> Self {
        Self {
            description: Some(description.into()),
            ..Self::default()
        }
    }

    /// Full metadata: description, instructions, and aliases.
    pub fn new(
        description: Option<String>,
        instructions: Option<String>,
        aliases: Vec<String>,
    ) -> Self {
        Self {
            description,
            instructions,
            aliases,
        }
    }
}

/// A namespace's unique key in the space: its joined path, e.g. `github` or
/// `mcp__docs`.
pub type NamespaceKey = String;

/// Approval/undo annotations for one tool (Cloudflare's annotations).
/// The shape is stored so hosts can honor it; the engine treats every call
/// identically — pausing is the host's decision at the [`HostCall`] boundary.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
#[non_exhaustive]
pub struct ToolAnnotations {
    /// The host should pause the run for approval before executing.
    pub requires_approval: bool,
    /// The call is idempotent and large; a replay re-executes instead of
    /// replaying a recorded result (CF's `replay: "reexecute"`).
    pub ephemeral: bool,
}

impl ToolAnnotations {
    /// Annotations marking the tool as requiring approval.
    pub fn requires_approval() -> Self {
        Self {
            requires_approval: true,
            ephemeral: false,
        }
    }
}

/// Schemas describing a tool's input and output, for signatures and
/// validation-free rendering (pi/OpenCode/CF all render-only here; hosts
/// own validation).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
#[non_exhaustive]
pub struct SchemaInfo {
    /// JSON Schema of the single input object.
    pub input: Option<Value>,
    /// JSON Schema of the resolved value. `describeTool` and the catalog
    /// render `Promise<T>` from it (pi renders output schemas; CF carries
    /// them on connectors).
    pub output: Option<Value>,
}

impl SchemaInfo {
    /// Schemas from input and output JSON Schema documents.
    pub fn new(input: Option<Value>, output: Option<Value>) -> Self {
        Self { input, output }
    }
}

/// The dispatcher a script's call reaches. Arguments and results cross as
/// JSON values; an `Err` rejects inside the script with the tool's error
/// text (pi's contract).
pub type ToolFn =
    Arc<dyn Fn(Value) -> futures::future::BoxFuture<'static, Result<Value, String>> + Send + Sync>;

/// One callable tool in the space.
#[derive(Clone)]
#[non_exhaustive]
pub struct ToolHandle {
    /// Path segments, e.g. `["github", "create_issue"]`. A flat spelling is
    /// one segment.
    pub segments: Vec<String>,
    /// Shown in listings, `ALL_TOOLS`, and search.
    pub description: Option<String>,
    /// Input/output schemas, for signatures.
    pub schema: SchemaInfo,
    /// Approval/undo annotations (CF).
    pub annotations: ToolAnnotations,
    /// How the tool is exposed.
    pub exposure: Exposure,
    /// The dispatcher.
    pub execute: ToolFn,
}

impl std::fmt::Debug for ToolHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolHandle")
            .field("segments", &self.segments)
            .field("description", &self.description)
            .field("schema", &self.schema)
            .field("annotations", &self.annotations)
            .field("exposure", &self.exposure)
            .finish_non_exhaustive()
    }
}

impl ToolHandle {
    /// A tool from segments, a description, and a dispatcher.
    pub fn new(segments: Vec<String>, description: impl Into<String>, execute: ToolFn) -> Self {
        Self {
            segments,
            description: Some(description.into()),
            schema: SchemaInfo::default(),
            annotations: ToolAnnotations::default(),
            exposure: Exposure::CodeModeOnly,
            execute,
        }
    }

    /// The canonical path as the space spells it: segments joined with `.`.
    /// This is also the registry key.
    pub fn canonical(&self) -> String {
        self.segments.join(".")
    }
}

/// Validate path segments: non-empty, and no segment is empty after
/// trimming (an empty segment would corrupt the canonical path).
pub fn validate_segments(segments: &[String]) -> Result<(), String> {
    if segments.is_empty() {
        return Err("tool path needs at least one segment".to_owned());
    }
    for segment in segments {
        if segment.is_empty() {
            return Err(format!(
                "tool path {segments:?} has an empty segment (OpenCode: a name may not contain an empty segment)"
            ));
        }
    }
    Ok(())
}

/// Join segments the way a script writes a flat identifier (pi's
/// `toCodemodeIdentifier`): characters outside identifiers become `_`.
/// Segments join with `__` (pi's spelling: namespace `mcp__docs` + tool
/// `search` is `mcp__docs__search`); each segment is identifier-normalized
/// first, so `my-tool` contributes `my_tool`.
pub fn flat_identifier(segments: &[String]) -> String {
    let joined = segments.join("__");
    crate::identity::normalize_identifier(&joined)
}

/// Split a canonical path back into segments. A flat identifier produced by
/// [`flat_identifier`] cannot be split back — the space keys on segments.
pub fn resolve_path(path: &str) -> Vec<String> {
    path.split('.').map(str::to_owned).collect()
}

/// The tool space: every entry and namespace, keyed canonically. Iteration
/// is in sorted path order, so listings are deterministic across restarts.
#[derive(Clone, Default)]
pub struct Space {
    entries: BTreeMap<String, ToolHandle>,
    namespaces: BTreeMap<NamespaceKey, NamespaceMeta>,
}

impl std::fmt::Debug for Space {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Space")
            .field("tools", &self.entries.keys().collect::<Vec<_>>())
            .field("namespaces", &self.namespaces.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Space {
    /// An empty space.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a tool. Errors when the canonical path is already taken
    /// (Cloudflare's connector does the same; pi hash-suffixes — hosts that
    /// want that call [`Space::entry`] first and rename).
    pub fn insert(&mut self, tool: ToolHandle) -> Result<(), String> {
        validate_segments(&tool.segments)?;
        let canonical = tool.canonical();
        if self.entries.contains_key(&canonical) {
            return Err(format!("duplicate tool path `{canonical}`"));
        }
        // Register every intermediate segment as a namespace so metadata and
        // enumeration work without the host declaring them (OpenCode: the
        // record's shape is the namespace).
        for depth in 1..tool.segments.len() {
            let key = tool.segments[..depth].join(".");
            self.namespaces.entry(key).or_default();
        }
        self.entries.insert(canonical, tool);
        Ok(())
    }

    /// Attach or replace namespace metadata. Returns whether any tool lives
    /// under the namespace.
    pub fn set_namespace(&mut self, key: NamespaceKey, meta: NamespaceMeta) -> bool {
        let populated = self
            .entries
            .keys()
            .any(|path| path.starts_with(&format!("{key}.")));
        self.namespaces.insert(key, meta);
        populated
    }

    /// A tool by canonical path.
    pub fn entry(&self, path: &str) -> Option<&ToolHandle> {
        self.entries.get(path)
    }

    /// Every tool, in sorted canonical order.
    pub fn entries(&self) -> impl Iterator<Item = &ToolHandle> {
        self.entries.values()
    }

    /// The number of tools.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the space is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Metadata for one namespace key.
    pub fn namespace(&self, key: &str) -> Option<&NamespaceMeta> {
        self.namespaces.get(key)
    }

    /// Every declared namespace key, sorted.
    pub fn namespace_keys(&self) -> impl Iterator<Item = &NamespaceKey> {
        self.namespaces.keys()
    }

    /// Resolve a script-supplied namespace name to its canonical key: the
    /// name itself, then a namespace whose own key or alias matches as
    /// written or as the same script identifier (pi's `isNamespaceName`),
    /// then a plain path prefix of the tree.
    pub fn resolve_namespace(&self, name: &str) -> Option<NamespaceKey> {
        if self.namespaces.contains_key(name) {
            return Some(name.to_owned());
        }
        let normalized = crate::identity::normalize_identifier(name);
        if let Some((key, _)) = self.namespaces.iter().find(|(canonical, meta)| {
            crate::identity::normalize_identifier(canonical) == normalized
                || meta
                    .aliases
                    .iter()
                    .any(|alias| crate::identity::normalize_identifier(alias) == normalized)
        }) {
            return Some(key.clone());
        }
        // Path-prefix fallback: a tree namespace without declared metadata.
        let prefix = format!("{name}.");
        self.entries
            .keys()
            .any(|path| path.starts_with(&prefix))
            .then(|| name.to_owned())
    }

    /// The tools of one namespace: everything whose canonical path starts
    /// with `{key}.`, in sorted order.
    pub fn namespace_members(&self, key: &str) -> Vec<&ToolHandle> {
        let prefix = format!("{key}.");
        self.entries
            .values()
            .filter(|tool| tool.canonical().starts_with(&prefix))
            .collect()
    }

    /// Resolve a script-supplied tool path (as written, e.g.
    /// `github.create_issue` or a flat identifier under a flat projection)
    /// to a tool. Matching is canonical first, then script-identifier equal
    /// (pi: `tools.mcp__docs__search` and `tools["mcp__docs__search"]` are
    /// the same tool).
    pub fn resolve_tool(&self, path: &str) -> Option<&ToolHandle> {
        if let Some(tool) = self.entries.get(path) {
            return Some(tool);
        }
        let normalized = crate::identity::normalize_identifier(path);
        self.entries
            .values()
            .find(|tool| crate::identity::normalize_identifier(&tool.canonical()) == normalized)
    }

    /// A search index over the space, rebuilt on demand. Long-lived hosts
    /// should hold one and rebuild on registration changes; the cost is
    /// linear in tools.
    pub fn search_index(&self) -> crate::search_shim::SearchIndex {
        let mut index = crate::search_shim::SearchIndex::default();
        for tool in self.entries.values() {
            let mut text = Vec::new();
            for depth in 1..tool.segments.len() {
                let key = tool.segments[..depth].join(".");
                if let Some(meta) = self.namespaces.get(&key) {
                    if let Some(description) = &meta.description {
                        text.push(description.clone());
                    }
                    if let Some(instructions) = &meta.instructions {
                        text.push(instructions.clone());
                    }
                }
            }
            index.add(&tool.canonical(), tool.description.as_deref(), &text);
        }
        index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(segments: &[&str], description: &str) -> ToolHandle {
        ToolHandle::new(
            segments.iter().map(|s| (*s).to_owned()).collect(),
            description,
            Arc::new(|_| Box::pin(async { Ok(Value::Null) })),
        )
    }

    #[test]
    fn hierarchical_insert_registers_intermediate_namespaces() {
        let mut space = Space::new();
        space
            .insert(tool(&["github", "issues", "list"], "List"))
            .unwrap();
        assert!(space.namespace("github").is_some());
        assert!(space.namespace("github.issues").is_some());
        assert_eq!(space.namespace_members("github").len(), 1);
        assert_eq!(space.namespace_members("github.issues").len(), 1);
    }

    #[test]
    fn duplicate_paths_fail() {
        let mut space = Space::new();
        space.insert(tool(&["a", "b"], "one")).unwrap();
        let error = space.insert(tool(&["a", "b"], "two")).unwrap_err();
        assert!(error.contains("duplicate"), "{error}");
    }

    #[test]
    fn flat_paths_are_single_segments() {
        let mut space = Space::new();
        space
            .insert(tool(&["mcp__docs__search"], "Search docs"))
            .unwrap();
        let resolved = space.resolve_tool("mcp__docs__search").unwrap();
        assert_eq!(resolved.segments, ["mcp__docs__search"]);
        // No intermediate namespaces exist for a one-segment tool.
        assert!(space.namespace_keys().next().is_none());
    }

    #[test]
    fn flat_identifier_spells_pi_style_names() {
        assert_eq!(
            flat_identifier(&["mcp__docs".to_owned(), "search".to_owned()]),
            "mcp__docs__search"
        );
        assert_eq!(
            flat_identifier(&["github".to_owned(), "my-tool".to_owned()]),
            "github__my_tool"
        );
    }

    #[test]
    fn resolve_namespace_matches_aliases_and_identifier_forms() {
        let mut space = Space::new();
        space
            .insert(tool(&["mcp__docs", "search"], "Search"))
            .unwrap();
        space.set_namespace(
            "mcp__docs".to_owned(),
            NamespaceMeta {
                aliases: vec!["docs".to_owned()],
                ..Default::default()
            },
        );
        assert_eq!(
            space.resolve_namespace("docs").as_deref(),
            Some("mcp__docs")
        );
        assert_eq!(
            space.resolve_namespace("mcp__docs").as_deref(),
            Some("mcp__docs")
        );
        // Tree namespaces without metadata resolve by prefix.
        space
            .insert(tool(&["api", "issues", "list"], "List"))
            .unwrap();
        assert_eq!(space.resolve_namespace("api").as_deref(), Some("api"));
        assert_eq!(space.resolve_namespace("nope"), None);
    }

    #[test]
    fn resolve_tool_matches_identifier_forms() {
        let mut space = Space::new();
        space.insert(tool(&["my-tool"], "Tool")).unwrap();
        assert!(space.resolve_tool("my_tool").is_some());
        assert!(space.resolve_tool("my-tool").is_some());
        assert!(space.resolve_tool("other").is_none());
    }
}
