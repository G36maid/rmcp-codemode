//! The catalog: what the model sees.
//!
//! One view over a [`Space`] answering the three discovery questions every
//! design shares — what is listed inline, what search returns, what one
//! target's details are:
//!
//! - **Listings** (pi's `createCodemodeDescription`): tools grouped by
//!   namespace, TypeScript declarations, limited to an inline token budget
//!   with pi's round-robin selection — in each round every group places its
//!   cheapest remaining tool, so every namespace is represented before any
//!   namespace is complete.
//! - **Search** (OpenCode's shape, pi's ranking): `{ items, remaining, next
//!   }` paging over BM25-ranked matches, scoped by namespace, resolved
//!   through aliases.
//! - **Describe** (pi + CF): one tool's full declaration, or a namespace's
//!   `{ name, description?, instructions?, tools }` through aliases.

use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    budget::Budget,
    identity::unknown_member_message,
    search_shim::{SearchHit, SearchIndex},
    signature::ts_declaration,
    space::{Exposure, Space, ToolHandle},
};

/// How listed tools are presented (pi's `codemode.mode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Direct tools stay in `tools/list` and are additionally declared in
    /// the codemode description (pi's `on`, the default).
    #[default]
    On,
    /// Direct tools are hidden from `tools/list` and listed in the codemode
    /// description instead (pi's `only`, OpenCode 2.0's MCP default).
    Only,
}

/// One search hit as the script sees it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SearchItem {
    /// The tool's canonical path.
    pub name: String,
    /// The tool's description as indexed.
    pub description: String,
}

/// A paged search result (OpenCode's contract).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SearchPage {
    /// This page's hits, in rank order.
    pub items: Vec<SearchItem>,
    /// Hits not included in this or any earlier page.
    pub remaining: usize,
    /// The `offset` for the next page, or `null` when exhausted.
    pub next: Option<usize>,
}

/// One tool's full model-facing description.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DescribedTool {
    /// Canonical path.
    pub name: String,
    /// Description (and prompt guidelines, when the host attaches them).
    pub description: String,
    /// TypeScript declaration rendered from the schemas.
    pub declaration: String,
    /// Approval annotation, when the host marked it (CF).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub requires_approval: bool,
}

/// A namespace's model-facing description (pi's `describeNamespace` shape).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DescribedNamespace {
    /// Canonical namespace key.
    pub name: String,
    /// Shown with the group in listings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Longer usage guidance; never in listings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// The member tools' declarations.
    pub tools: Vec<DescribedTool>,
}

/// What [`Catalog::describe`] found.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::exhaustive_enums)] // tool and namespace exhaust discovery
pub enum Described {
    /// A single tool.
    Tool(DescribedTool),
    /// A namespace and its members.
    Namespace(DescribedNamespace),
}

/// One listed tool's rendered section and its token cost.
struct CatalogEntry {
    name: String,
    section: String,
    cost: usize,
}

/// Tools grouped under one namespace (or ungrouped).
struct CatalogGroup {
    /// `None` for ungrouped tools (pi: they come first).
    namespace: Option<(String, crate::space::NamespaceMeta)>,
    entries: Vec<CatalogEntry>,
}

/// The model-facing view over a [`Space`].
pub struct Catalog<'a> {
    space: &'a Space,
    budget: &'a Budget,
    index: SearchIndex,
}

impl<'a> Catalog<'a> {
    /// Build a catalog view. The search index is built once here; rebuild by
    /// calling again after the space changes.
    pub fn new(space: &'a Space, budget: &'a Budget) -> Self {
        Self {
            space,
            budget,
            index: space.search_index(),
        }
    }

    /// The tools that participate in listings: everything except the
    /// codemode tool itself, filtered by mode (pi's `prepareLoadout` rule:
    /// listing by exposure keeps the description stable when a `tool_search`
    /// loads a deferred tool).
    fn listed(&self, mode: Mode) -> Vec<&'a ToolHandle> {
        self.space
            .entries()
            .filter(|tool| match mode {
                // In `only` mode direct tools are listed too (hidden from
                // tools/list instead); in `on` mode direct tools stay
                // declared natively and deferred/codemode tools are listed.
                Mode::On => !matches!(tool.exposure, Exposure::Direct),
                Mode::Only => true,
            })
            .collect()
    }

    /// Render one tool's section: `### \`id\` (\`raw\`)` + description +
    /// declaration (pi's `renderToolSection`).
    fn section(&self, tool: &ToolHandle) -> String {
        let id = crate::identity::normalize_identifier(&tool.canonical());
        let canonical = tool.canonical();
        let heading = if id == canonical {
            format!("### `{id}`")
        } else {
            format!("### `{id}` (`{canonical}`)")
        };
        let declaration = ts_declaration(
            &canonical,
            tool.description.as_deref(),
            tool.schema.input.as_ref(),
            tool.schema.output.as_ref(),
        );
        let description = tool.description.clone().unwrap_or_default();
        format!("{heading}\n{description}\n\n{declaration}")
    }

    /// The inline budget (characters) for tool sections: pi's
    /// `codemode.inlineBudget` default of 3000 estimated tokens, i.e. 12000
    /// characters at 4 characters per token.
    fn inline_budget_chars(&self) -> Option<usize> {
        Some(3_000 * Budget::CHARS_PER_TOKEN)
    }

    /// The codemode tool's description: intro, globals line, and one section
    /// per listed tool, grouped by namespace, within the inline budget using
    /// pi's round-robin selection. Tools that do not fit are omitted with a
    /// pointer to `searchTools`.
    pub fn description(&self, mode: Mode) -> String {
        let listed = self.listed(mode);
        let mut groups: Vec<CatalogGroup> = vec![CatalogGroup {
            namespace: None,
            entries: Vec::new(),
        }];
        // Group by namespace in sorted order (pi: ungrouped first, then
        // namespaces by name).
        let mut namespace_order: Vec<String> = Vec::new();
        for tool in &listed {
            let key = tool
                .segments
                .split_last()
                .filter(|(_, rest)| !rest.is_empty())
                .map(|(_, rest)| rest.join("."));
            match key {
                Some(namespace) => {
                    if !namespace_order.contains(&namespace) {
                        namespace_order.push(namespace.clone());
                    }
                }
                None => {}
            }
        }
        namespace_order.sort();
        for namespace in &namespace_order {
            groups.push(CatalogGroup {
                namespace: Some((
                    namespace.clone(),
                    self.space.namespace(namespace).cloned().unwrap_or_default(),
                )),
                entries: Vec::new(),
            });
        }
        for tool in &listed {
            let section = self.section(tool);
            // Costs and the budget are both in characters (pi estimates
            // tokens as characters / 4; comparing in one unit avoids a
            // mixed-unit budget).
            let cost = section.len();
            let entry = CatalogEntry {
                name: tool.canonical(),
                section,
                cost,
            };
            let key = tool
                .segments
                .split_last()
                .filter(|(_, rest)| !rest.is_empty())
                .map(|(_, rest)| rest.join("."));
            let group = match key {
                Some(namespace) => groups
                    .iter_mut()
                    .find(|group| matches!(&group.namespace, Some((name, _)) if name == &namespace))
                    .expect("namespace group pre-registered"),
                None => &mut groups[0],
            };
            group.entries.push(entry);
        }

        // pi's round-robin budget selection: each round, every group places
        // its cheapest remaining section; a group whose next section does
        // not fit drops out while the others continue.
        let shown = match self.inline_budget_chars() {
            None => groups
                .iter()
                .flat_map(|group| group.entries.iter().map(|entry| entry.name.clone()))
                .collect::<std::collections::BTreeSet<_>>(),
            Some(budget) => {
                let mut queues: Vec<Vec<&CatalogEntry>> = groups
                    .iter()
                    .map(|group| {
                        let mut entries: Vec<&CatalogEntry> = group.entries.iter().collect();
                        entries.sort_by_key(|entry| entry.cost);
                        entries
                    })
                    .collect();
                let mut shown = std::collections::BTreeSet::new();
                let mut remaining = budget;
                loop {
                    queues.retain_mut(|queue| {
                        let Some(next) = queue.first() else {
                            return false;
                        };
                        if next.cost > remaining {
                            return false;
                        }
                        remaining -= next.cost;
                        shown.insert(next.name.clone());
                        queue.remove(0);
                        !queue.is_empty()
                    });
                    if queues.is_empty() {
                        break;
                    }
                }
                shown
            }
        };

        let mut sections = vec![
            "Run JavaScript that calls tools. Only the script's return value, `text()` output, \
             and `console` lines reach the model — intermediate tool results stay inside the \
             script."
                .to_owned(),
            "Globals: `tools.*`, `text(value)`, `image(dataUri)`, `console.log(...)`, `exit()`, \
             `store(key, value)` / `load(key)`, `ALL_TOOLS`, \
             `searchTools(query, { limit?, namespace?, offset? })`, `describeTool(name)`, \
             `describeNamespace(name)`. Optional first line: \
             `// @options: {\"max_output_tokens\": ..., \"timeout_ms\": ...}`."
                .to_owned(),
        ];
        let total_listed: usize = groups.iter().map(|group| group.entries.len()).sum();
        if total_listed > 0 {
            let mut tool_sections = vec!["Nested tools:".to_owned()];
            let mut omitted = 0usize;
            for group in &groups {
                if group.entries.is_empty() {
                    continue;
                }
                let mut members = String::new();
                if let Some((name, meta)) = &group.namespace {
                    let heading = format!("## {name}");
                    let visible = group
                        .entries
                        .iter()
                        .filter(|entry| shown.contains(&entry.name))
                        .count();
                    let listing = if visible == group.entries.len() {
                        ""
                    } else if visible == 0 {
                        " (tools not listed)"
                    } else {
                        " (some tools not listed)"
                    };
                    members.push_str(&format!("{heading}{listing}\n"));
                    if let Some(description) = &meta.description {
                        members.push_str(description);
                        members.push('\n');
                    }
                }
                for entry in &group.entries {
                    if shown.contains(&entry.name) {
                        members.push_str(&entry.section);
                        members.push('\n');
                    } else {
                        omitted += 1;
                    }
                }
                tool_sections.push(members);
            }
            if omitted > 0 {
                tool_sections.push(format!(
                    "{omitted} more tools exceed the inline declaration budget; call them via \
                     `tools.<path>(...)` and find them with `searchTools` or `describeTool`."
                ));
            }
            sections.push(tool_sections.join("\n"));
        }
        sections.join("\n\n")
    }

    /// Ranked, paged search over the space (pi's BM25 ranking, OpenCode's
    /// `{ items, remaining, next }` contract). The namespace resolves
    /// through aliases; an empty query returns tools in path order.
    pub fn search(
        &self,
        query: &str,
        namespace: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> SearchPage {
        let canonical_namespace = namespace.and_then(|ns| self.space.resolve_namespace(ns));
        let scoped: Vec<SearchHit> = match &canonical_namespace {
            Some(key) => {
                let prefix = format!("{key}.");
                self.space
                    .entries()
                    .filter(|tool| tool.canonical().starts_with(&prefix))
                    .map(|tool| SearchHit {
                        path: tool.canonical(),
                        description: tool.description.clone().unwrap_or_default(),
                        score: 0.0,
                    })
                    .collect()
            }
            None => Vec::new(),
        };
        let hits: Vec<SearchHit> = if let Some(key) = &canonical_namespace {
            // Namespace-scoped: rank only the members.
            let mut index = SearchIndex::default();
            for tool in self.space.namespace_members(key) {
                index.add(&tool.canonical(), tool.description.as_deref(), &[]);
            }
            index.search(query, usize::MAX)
        } else if scoped.is_empty() && canonical_namespace.is_some() {
            Vec::new()
        } else {
            self.index.search(query, usize::MAX)
        };
        let page: Vec<SearchHit> = hits.iter().skip(offset).take(limit).cloned().collect();
        let remaining = hits.len().saturating_sub(offset + page.len());
        let next = (remaining > 0).then(|| offset + page.len());
        SearchPage {
            items: page
                .into_iter()
                .map(|hit| SearchItem {
                    name: hit.path,
                    description: hit.description,
                })
                .collect(),
            remaining,
            next,
        }
    }

    /// Describe one target: a tool (canonical path or identifier form), or a
    /// namespace through its key or aliases (pi). Unknown targets resolve to
    /// `None` — the engine turns that into the guard message.
    pub fn describe(&self, target: &str) -> Option<Described> {
        if let Some(tool) = self.space.resolve_tool(target) {
            return Some(Described::Tool(DescribedTool {
                name: tool.canonical(),
                description: tool.description.clone().unwrap_or_default(),
                declaration: ts_declaration(
                    &tool.canonical(),
                    tool.description.as_deref(),
                    tool.schema.input.as_ref(),
                    tool.schema.output.as_ref(),
                ),
                requires_approval: tool.annotations.requires_approval,
            }));
        }
        let key = self.space.resolve_namespace(target)?;
        let members = self.space.namespace_members(&key);
        if members.is_empty() {
            return None;
        }
        let meta = self.space.namespace(&key).cloned().unwrap_or_default();
        Some(Described::Namespace(DescribedNamespace {
            name: key,
            description: meta.description,
            instructions: meta.instructions,
            tools: members
                .iter()
                .map(|tool| DescribedTool {
                    name: tool.canonical(),
                    description: tool.description.clone().unwrap_or_default(),
                    declaration: ts_declaration(
                        &tool.canonical(),
                        tool.description.as_deref(),
                        tool.schema.input.as_ref(),
                        tool.schema.output.as_ref(),
                    ),
                    requires_approval: tool.annotations.requires_approval,
                })
                .collect(),
        }))
    }

    /// The error text for an unknown tool path (pi's guard message, with the
    /// search hint).
    pub fn unknown_tool_message(&self, path: &str) -> String {
        let names: Vec<String> = self.space.entries().map(|tool| tool.canonical()).collect();
        unknown_member_message(
            "tools",
            path,
            &names,
            "ALL_TOOLS lists every tool; searchTools(query) finds tools by topic.",
        )
    }
}

/// The `searchTools` result's JSON, as the bridge hands it to the script.
pub fn search_page_json(page: &SearchPage) -> Value {
    json!({
        "items": page.items,
        "remaining": page.remaining,
        "next": page.next,
    })
}
