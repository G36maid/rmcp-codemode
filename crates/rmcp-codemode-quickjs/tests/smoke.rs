//! Minimal smoke tests for the execute() path — the full pi-test port
//! follows in tests/sandbox.rs.

use std::sync::Arc;

use rmcp_codemode::{
    engine::Engine,
    types::{ExecuteOptions, OutputItem, SandboxOptions},
};
use rmcp_codemode_quickjs::QuickJsEngine;
use serde_json::json;

fn engine() -> QuickJsEngine {
    QuickJsEngine::new(SandboxOptions::default())
}

#[tokio::test(flavor = "current_thread")]
async fn returns_value_after_json_round_trip() {
    let result = engine()
        .execute("return { a: 1, b: [true, 'x'] }", ExecuteOptions::default())
        .await;
    assert_eq!(
        result.outcome.as_ref().unwrap().value,
        json!({"a": 1, "b": [true, "x"]})
    );
    assert!(result.output.is_empty());
    assert!(result.calls.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn empty_script_returns_undefined() {
    let result = engine().execute("", ExecuteOptions::default()).await;
    assert_eq!(result.outcome.unwrap().value, serde_json::Value::Null);
}

#[tokio::test(flavor = "current_thread")]
async fn tool_call_flows_and_records() {
    let tools = vec![Tool {
        name: "mcp__cat__echo".into(),
        description: Some("echoes".into()),
        input_schema: None,
        output_schema: None,
        spread: false,
        signature: None,
        execute: Arc::new(|args, _ctx| Box::pin(async move { Ok(json!({ "echoed": args })) })),
    }];
    let sandbox = SandboxOptions {
        tools,
        ..SandboxOptions::default()
    };
    let result = QuickJsEngine::new(sandbox)
        .execute(
            "const r = await tools.mcp__cat__echo({ n: 1 }); return r.echoed.n",
            ExecuteOptions::default(),
        )
        .await;
    assert_eq!(result.outcome.unwrap().value, json!(1));
    assert_eq!(result.calls.len(), 1);
    assert_eq!(result.calls[0].name, "mcp__cat__echo");
    assert_eq!(result.calls[0].status, rmcp_codemode::types::CallStatus::Ok);
}

use rmcp_codemode::types::Tool;

#[tokio::test(flavor = "current_thread")]
async fn console_and_text_collect_in_order() {
    let result = engine()
        .execute(
            r#"console.log("hello", 1, { a: 1 }); text({ json: true }); text(7); return null"#,
            ExecuteOptions::default(),
        )
        .await;
    assert!(result.outcome.is_ok());
    assert_eq!(
        result.output,
        vec![
            OutputItem::Text {
                text: r#"hello 1 {"a":1}"#.into(),
                console: true
            },
            OutputItem::Text {
                text: r#"{"json":true}"#.into(),
                console: false
            },
            OutputItem::Text {
                text: "7".into(),
                console: false
            },
        ]
    );
}
