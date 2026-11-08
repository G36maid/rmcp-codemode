//! Catalog semantics: pi's round-robin inline budget, OpenCode's paging,
//! pi's describe shape with aliases and instructions.

use std::sync::Arc;

use rmcp_codemode::{Budget, Catalog, Exposure, Mode, NamespaceMeta, Space, ToolHandle};
use serde_json::{Value, json};

fn tool(path: &str, description: &str) -> ToolHandle {
    ToolHandle::new(
        path.split('.').map(str::to_owned).collect(),
        description,
        Arc::new(|_| Box::pin(async { Ok(Value::Null) })),
    )
}

fn space_with_big_namespaces() -> Space {
    // Three namespaces, each with tools of escalating description length —
    // the round-robin budget must interleave, not fill one namespace first.
    let mut space = Space::new();
    for namespace in ["alpha", "beta", "gamma"] {
        space.set_namespace(
            namespace.to_owned(),
            NamespaceMeta::with_description(format!("The {namespace} API")),
        );
        for i in 0..4 {
            space
                .insert(tool(
                    &format!("{namespace}.tool_{i}"),
                    &format!("Tool {i} of {namespace}. {}", "x".repeat(i * 400)),
                ))
                .unwrap();
        }
    }
    space
}

#[test]
fn round_robin_budget_represents_every_namespace_first() {
    let space = space_with_big_namespaces();
    let budget = Budget::default().with_max_output_tokens(700);
    let catalog = Catalog::new(&space, &budget);
    let description = catalog.description(Mode::On);

    // Every namespace appears (pi: "Every namespace is represented before
    // any namespace is complete") — even though 12 tools exceed the budget.
    for namespace in ["alpha", "beta", "gamma"] {
        assert!(
            description.contains(&format!("## {namespace}")),
            "{description}"
        );
    }
    assert!(description.contains("exceed the inline declaration budget"));
}

#[test]
fn search_pages_and_resolves_namespace_aliases() {
    let mut space = Space::new();
    space
        .insert(tool("mcp__docs.search", "Search documentation"))
        .unwrap();
    space
        .insert(tool("mcp__docs.read", "Read a documentation page"))
        .unwrap();
    space.insert(tool("other.thing", "Something else")).unwrap();
    space.set_namespace(
        "mcp__docs".to_owned(),
        NamespaceMeta::new(
            Some("Documentation tools".to_owned()),
            None,
            vec!["docs".to_owned()],
        ),
    );
    let budget = Budget::default();
    let catalog = Catalog::new(&space, &budget);

    // Page one: one item, one remaining, a next offset.
    let page = catalog.search("", Some("docs"), 1, 0);
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.remaining, 1);
    assert_eq!(page.next, Some(1));

    // Page two via next: no overlap, exhausted.
    let page2 = catalog.search("", Some("docs"), 1, page.next.unwrap());
    assert_eq!(page2.items.len(), 1);
    assert_eq!(page2.remaining, 0);
    assert_eq!(page2.next, None);
    assert_ne!(page.items[0].name, page2.items[0].name);

    // Alias scoping excludes the unrelated tool.
    let all = catalog.search("", Some("docs"), 10, 0);
    assert!(
        all.items
            .iter()
            .all(|item| item.name.starts_with("mcp__docs"))
    );
}

#[test]
fn describe_returns_namespace_with_instructions_through_alias() {
    let mut space = Space::new();
    space
        .insert(tool("mcp__docs.search", "Search docs"))
        .unwrap();
    space.set_namespace(
        "mcp__docs".to_owned(),
        NamespaceMeta::new(
            Some("Documentation search".to_owned()),
            Some("Use for repository documentation.".to_owned()),
            vec!["docs".to_owned()],
        ),
    );
    let budget = Budget::default();
    let catalog = Catalog::new(&space, &budget);

    let described = catalog.describe("docs").expect("alias resolves");
    let rmcp_codemode::Described::Namespace(namespace) = described else {
        panic!("expected a namespace");
    };
    assert_eq!(namespace.name, "mcp__docs");
    assert_eq!(
        namespace.description.as_deref(),
        Some("Documentation search")
    );
    assert_eq!(
        namespace.instructions.as_deref(),
        Some("Use for repository documentation.")
    );
    assert_eq!(namespace.tools.len(), 1);
    assert!(
        namespace.tools[0]
            .declaration
            .contains("declare function search(")
    );
}

#[test]
fn describe_tool_carries_approval_annotation() {
    use rmcp_codemode::ToolAnnotations;
    let mut space = Space::new();
    let mut handle = tool("github.create_issue", "Create an issue");
    handle.annotations = ToolAnnotations::requires_approval();
    space.insert(handle).unwrap();
    let budget = Budget::default();
    let catalog = Catalog::new(&space, &budget);
    let described = catalog.describe("github.create_issue").unwrap();
    let rmcp_codemode::Described::Tool(tool) = described else {
        panic!("expected a tool");
    };
    assert!(tool.requires_approval);
}

#[test]
fn unknown_tool_gets_pi_guard_message() {
    let mut space = Space::new();
    space.insert(tool("web_search", "Search the web")).unwrap();
    space.insert(tool("echo", "Echo")).unwrap();
    let budget = Budget::default();
    let catalog = Catalog::new(&space, &budget);
    let message = catalog.unknown_tool_message("websearch");
    assert!(
        message.contains("Did you mean tools.web_search?"),
        "{message}"
    );
    assert!(message.contains("searchTools(query)"), "{message}");
}

#[test]
fn mode_only_lists_direct_tools_too() {
    let mut space = Space::new();
    let mut direct = tool("fs.read", "Read a file");
    direct.exposure = Exposure::Direct;
    space.insert(direct).unwrap();
    space
        .insert(tool("github.search", "Search GitHub"))
        .unwrap();
    let budget = Budget::default();
    let catalog = Catalog::new(&space, &budget);

    // On: only non-direct tools in the codemode description.
    let on = catalog.description(Mode::On);
    assert!(on.contains("github.search"));
    assert!(!on.contains("### `fs_read`"), "{on}");

    // Only: everything, including direct tools (hidden from tools/list).
    let only = catalog.description(Mode::Only);
    assert!(only.contains("github.search"));
    assert!(only.contains("fs.read"));
}

#[test]
fn search_json_shape_matches_opencode() {
    let mut space = Space::new();
    space.insert(tool("a.b", "B")).unwrap();
    space.insert(tool("a.c", "C")).unwrap();
    let budget = Budget::default();
    let catalog = Catalog::new(&space, &budget);
    let page = catalog.search("", None, 1, 0);
    let value = rmcp_codemode::catalog::search_page_json(&page);
    assert_eq!(value["remaining"], json!(1));
    assert_eq!(value["next"], json!(1));
    assert_eq!(value["items"].as_array().unwrap().len(), 1);
}
