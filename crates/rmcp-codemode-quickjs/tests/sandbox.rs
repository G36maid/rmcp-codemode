//! Ported from pi's `packages/codemode/test/sandbox.test.ts` — the conformance
//! oracle (SPEC §11.1). Each test is labeled with its pi source case. Same
//! scripts, same fixtures, same assertions (adapted to the Rust contract
//! types); a failing port is an engine bug, not a test bug.

use std::sync::Arc;

use rmcp_codemode::{
    engine::Engine,
    types::{ExecuteOptions, SandboxOptions, Tool},
};
use rmcp_codemode_quickjs::QuickJsEngine;
use serde_json::json;

// pi sandbox.test.ts:62-65 — base64 of the leading bytes of each format;
// image() only inspects the signature.
const PNG: &str = "iVBORw0KGgo=";
const JPEG: &str = "/9j/4A==";
const GIF: &str = "R0lGODlh";
const WEBP: &str = "UklGRgAAAABXRUJQ";

fn create_sandbox(tools: Vec<Tool>) -> QuickJsEngine {
    // pi: createSandbox(tools, timeoutMs = 10_000)
    QuickJsEngine::new(SandboxOptions {
        tools,
        timeout_ms: Some(10_000),
        ..SandboxOptions::default()
    })
}

// pi: `const echo: CodemodeTool = { name: "echo", execute: (args) => args };`
// (used by later ported cases: tools > exposes tools as async functions).
#[allow(dead_code)]
fn echo_tool() -> Tool {
    Tool {
        name: "echo".into(),
        description: None,
        input_schema: None,
        output_schema: None,
        spread: false,
        signature: None,
        execute: Arc::new(|args, _ctx| Box::pin(async move { Ok(args) })),
    }
}

// pi: sandbox.test.ts > script execution > returns the script's return value
// after a JSON round trip.
#[tokio::test(flavor = "current_thread")]
async fn returns_return_value_after_json_round_trip() {
    let sandbox = create_sandbox(vec![]);
    let result = sandbox
        .execute("return { a: 1, b: [true, 'x'] }", ExecuteOptions::default())
        .await;
    let ok = result.outcome.as_ref().unwrap();
    assert_eq!(ok.value, json!({"a": 1, "b": [true, "x"]}));
    assert!(result.output.is_empty());
    assert!(result.calls.is_empty());

    let result = sandbox
        .execute("return 'plain'", ExecuteOptions::default())
        .await;
    assert_eq!(result.outcome.as_ref().unwrap().value, json!("plain"));

    let result = sandbox.execute("", ExecuteOptions::default()).await;
    assert_eq!(result.outcome.as_ref().unwrap().value, json!(null));
}

// pi: sandbox.test.ts > script execution > supports top-level await.
#[tokio::test(flavor = "current_thread")]
async fn supports_top_level_await() {
    let sandbox = create_sandbox(vec![]);
    let result = sandbox
        .execute(
            "const x = await Promise.resolve(41); return x + 1",
            ExecuteOptions::default(),
        )
        .await;
    assert_eq!(result.outcome.as_ref().unwrap().value, json!(42));
}

// pi: sandbox.test.ts > script execution > collects text(), image(), and
// console output in order.
#[tokio::test(flavor = "current_thread")]
async fn collects_text_image_console_output_in_order() {
    use rmcp_codemode::types::OutputItem;
    let sandbox = create_sandbox(vec![]);
    let result = sandbox
        .execute(
            r#"
				console.log("hello", 1, { a: 1 });
				text({ json: true });
				text(undefined);
				text(7);
				image("data:image/png;base64,iVBORw0KGgo=");
				image({ image_url: "data:image/jpeg;base64,/9j/4A==" });
				image({ type: "image", data: "R0lGODlh", mimeType: "image/gif" });
				image("data:image/png;base64,UklGRgAAAABXRUJQ");
				image({ type: "image", data: "iVBORw0KGgo=" });
				console.error(new Error("bad"));
				return null;
			"#,
            ExecuteOptions::default(),
        )
        .await;
    assert!(result.outcome.is_ok());
    // slice(0, -1): all but the last (the Error console line).
    let items = &result.output[..result.output.len() - 1];
    let expected = vec![
        OutputItem::Text {
            text: r#"hello 1 {"a":1}"#.into(),
            console: true,
        },
        OutputItem::Text {
            text: r#"{"json":true}"#.into(),
            console: false,
        },
        OutputItem::Text {
            text: "undefined".into(),
            console: false,
        },
        OutputItem::Text {
            text: "7".into(),
            console: false,
        },
        OutputItem::Image {
            data: PNG.into(),
            mime_type: "image/png".into(),
        },
        OutputItem::Image {
            data: JPEG.into(),
            mime_type: "image/jpeg".into(),
        },
        OutputItem::Image {
            data: GIF.into(),
            mime_type: "image/gif".into(),
        },
        // The MIME type comes from the data, not from the declared type.
        OutputItem::Image {
            data: WEBP.into(),
            mime_type: "image/webp".into(),
        },
        OutputItem::Image {
            data: PNG.into(),
            mime_type: "image/png".into(),
        },
    ];
    assert_eq!(items, &expected[..]);
    // The last item: console.error(new Error("bad")) → "Error: bad", console.
    let last = result.output.last().unwrap();
    match last {
        OutputItem::Text { text, console } => {
            assert!(text.starts_with("Error: bad"), "{text}");
            assert!(*console);
        }
        other => panic!("expected text item, got {other:?}"),
    }
}

// pi: sandbox.test.ts > script execution > prints non-finite numbers in
// console output and thrown values like text() (#11901).
#[tokio::test(flavor = "current_thread")]
async fn prints_non_finite_numbers_like_text() {
    use rmcp_codemode::types::OutputItem;
    let sandbox = create_sandbox(vec![]);
    let logged = sandbox
        .execute(
            r#"console.log("avg", 0 / 0, 1 / 0, -1 / 0, { a: 1 }); text(0 / 0);"#,
            ExecuteOptions::default(),
        )
        .await;
    assert_eq!(
        logged.output,
        vec![
            OutputItem::Text {
                text: r#"avg NaN Infinity -Infinity {"a":1}"#.into(),
                console: true
            },
            OutputItem::Text {
                text: "NaN".into(),
                console: false
            },
        ]
    );

    // `throw NaN` → ok:false, error kind script, message "NaN".
    let thrown = sandbox
        .execute("throw NaN", ExecuteOptions::default())
        .await;
    let error = thrown.outcome.unwrap_err();
    assert_eq!(error.kind, rmcp_codemode::types::ErrorKind::Script);
    assert_eq!(error.message, "NaN");
}

// pi: sandbox.test.ts > script execution > rejects invalid text() and image()
// arguments. Every case is a distinct validation message in pi's prelude.
#[tokio::test(flavor = "current_thread")]
async fn rejects_invalid_text_and_image_arguments() {
    let sandbox = create_sandbox(vec![]);
    let result = sandbox
        .execute(
            r#"
				const errors = [];
				const circular = {};
				circular.self = circular;
				for (const run of [
					() => text(circular),
					() => image(""),
					() => image("https://example.com/a.png"),
					() => image("data:image/png,raw"),
					() => image({ type: "text", text: "x" }),
					() => image({ type: "image", data: "" }),
					() => image(42),
					() => image("data:image/png;base64,AAAA!"),
					() => image("data:image/png;base64,AAAAA"),
					() => image("data:image/png;base64,AA=A"),
					() => image("data:image/png;base64,"),
					() => image("data:image/png;base64,AAAA\\n[Output truncated]"),
					() => image({ type: "image", data: "AAAA!", mimeType: "image/png" }),
					() => image("data:image/png;base64,AAAA"),
					() => image("data:image/png;base64,QUJD"),
					() => image("data:image/jpeg;base64,/9j/9w=="),
				]) {
					try { run(); errors.push("no error"); } catch (error) { errors.push(error.name + ": " + error.message); }
				}
				return errors;
			"#,
            ExecuteOptions::default(),
        )
        .await;
    assert!(result.outcome.is_ok());
    assert!(result.output.is_empty());
    let errors = result.outcome.as_ref().unwrap().value.as_array().unwrap();
    let as_str = |v: &serde_json::Value| v.as_str().unwrap().to_owned();

    // First: text(circular) → TypeError mentioning "circular".
    let first = as_str(&errors[0]);
    assert!(
        first.starts_with("TypeError:") && first.contains("circular"),
        "{first}"
    );
    let expects_url = "TypeError: image expects a non-empty image URL string, an object with image_url, or an image block { type: \"image\", data, mimeType }";
    let bad_base64 = "TypeError: invalid image output. The image data is not valid base64 (truncated or corrupted?)";
    let not_image =
        "TypeError: invalid image output. The image data is not a PNG, JPEG, GIF, or WebP image";
    let expected = [
        expects_url.to_owned(),
        "TypeError: remote image URLs are not supported in tool outputs. Pass a base64 data URI instead".to_owned(),
        "TypeError: invalid image output. Pass a base64 data URI instead".to_owned(),
        "TypeError: image only accepts image blocks, got \"text\"".to_owned(),
        "TypeError: image expected image block data".to_owned(),
        expects_url.to_owned(),
        bad_base64.to_owned(),
        bad_base64.to_owned(),
        bad_base64.to_owned(),
        bad_base64.to_owned(),
        bad_base64.to_owned(),
        bad_base64.to_owned(),
        not_image.to_owned(),
        not_image.to_owned(),
        not_image.to_owned(),
    ];
    let got: Vec<String> = errors[1..].iter().map(as_str).collect();
    assert_eq!(got.len(), expected.len(), "{got:?}");
    for (i, (got, want)) in got.iter().zip(&expected).enumerate() {
        assert_eq!(got, want, "case {i}");
    }
}

// pi: sandbox.test.ts > script execution > accepts wrapped and large base64
// image data.
#[tokio::test(flavor = "current_thread")]
async fn accepts_wrapped_and_large_base64_image_data() {
    use rmcp_codemode::types::OutputItem;
    let sandbox = create_sandbox(vec![]);
    let large = format!("iVBORw0KGgoA{}", "QUJD".repeat(256 * 1024));
    let result = sandbox
        .execute(
            &(r#"image("data:image/png;base64,iVBORw0K\r\nGgo=\n");"#.to_owned()
                + &format!(r#"image("data:image/png;base64,{large}");"#)),
            ExecuteOptions::default(),
        )
        .await;
    if let Err(e) = &result.outcome {
        eprintln!("DBG wrapped-large error: {e:?}");
    }
    assert!(result.outcome.is_ok());
    assert_eq!(
        result.output,
        vec![
            OutputItem::Image {
                data: PNG.into(),
                mime_type: "image/png".into()
            },
            // Wrapped base64 is cleaned; large data passes through unchanged.
            OutputItem::Image {
                data: large,
                mime_type: "image/png".into()
            },
        ]
    );
}
