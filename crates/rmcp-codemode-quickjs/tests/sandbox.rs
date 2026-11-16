//! Ported from pi's `packages/codemode/test/sandbox.test.ts` — the conformance
//! oracle (SPEC §11.1). Each test is labeled with its pi source case. Same
//! scripts, same fixtures, same assertions (adapted to the Rust contract
//! types); a failing port is an engine bug, not a test bug.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use rmcp_codemode::{
    engine::Engine,
    types::{CallStatus, ErrorKind, ExecuteOptions, SandboxOptions, Tool, ToolError},
};
use rmcp_codemode_quickjs::QuickJsEngine;
use serde_json::{Value, json};

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

// ------------------------------------------------------- tools (pi 276+) ---

// pi: sandbox.test.ts > tools > exposes tools as async functions and records
// calls.
#[tokio::test(flavor = "current_thread")]
async fn exposes_tools_as_async_functions_and_records_calls() {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let seen2 = Arc::clone(&seen);
    let add = Tool {
        name: "add".into(),
        execute: Arc::new(move |args, _ctx| {
            let seen = Arc::clone(&seen2);
            Box::pin(async move {
                seen.lock().unwrap().push(args.clone());
                let a = args.get("a").and_then(Value::as_i64).unwrap_or(0);
                let b = args.get("b").and_then(Value::as_i64).unwrap_or(0);
                Ok(json!({ "sum": a + b }))
            })
        }),
        ..echo_tool()
    };
    let result = create_sandbox(vec![add])
        .execute(
            r#"
				const first = await tools.add({ a: 1, b: 2 });
				const second = await tools.add({ a: first.sum, b: 10 });
				return second.sum;
			"#,
            ExecuteOptions::default(),
        )
        .await;
    assert_eq!(result.outcome.as_ref().unwrap().value, json!(13));
    assert_eq!(
        *seen.lock().unwrap(),
        vec![json!({"a": 1, "b": 2}), json!({"a": 3, "b": 10})]
    );
    let statuses: Vec<(String, CallStatus)> = result
        .calls
        .iter()
        .map(|call| (call.name.clone(), call.status))
        .collect();
    assert_eq!(
        statuses,
        vec![
            ("add".into(), CallStatus::Ok),
            ("add".into(), CallStatus::Ok)
        ]
    );
    // durationMs is u64 — non-negative by type.
    assert!(result.calls.iter().all(|call| call.name == "add"));
}

// pi: sandbox.test.ts > tools > runs concurrent calls and lists tool names.
#[tokio::test(flavor = "current_thread")]
async fn runs_concurrent_calls_and_lists_tool_names() {
    let delay = Tool {
        name: "delay".into(),
        execute: Arc::new(|args, _ctx| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok(args)
            })
        }),
        ..echo_tool()
    };
    let result = create_sandbox(vec![echo_tool(), delay])
        .execute(
            r#"
				const [a, b, c] = await Promise.all([tools.delay(1), tools.delay(2), tools.echo(3)]);
				return { values: [a, b, c], names: Object.keys(tools) };
			"#,
            ExecuteOptions::default(),
        )
        .await;
    assert_eq!(
        result.outcome.as_ref().unwrap().value,
        json!({"values": [1, 2, 3], "names": ["echo", "delay"]})
    );
}

// pi: sandbox.test.ts > tools > exposes tools under normalized identifiers
// and lists them in ALL_TOOLS. First tool wins a normalized-name collision;
// ALL_TOOLS is frozen (push throws, caught).
#[tokio::test(flavor = "current_thread")]
async fn exposes_normalized_identifiers_and_all_tools() {
    let dashes = Tool {
        name: "my-tool".into(),
        description: Some("Dashes".into()),
        execute: Arc::new(|_a, _c| Box::pin(async { Ok(json!("dash")) })),
        ..echo_tool()
    };
    let underscored = Tool {
        name: "my_tool".into(),
        description: Some("Shadowed".into()),
        execute: Arc::new(|_a, _c| Box::pin(async { Ok(json!("underscore")) })),
        ..echo_tool()
    };
    let mcp = Tool {
        name: "mcp__docs__search".into(),
        execute: Arc::new(|_a, _c| Box::pin(async { Ok(json!("mcp")) })),
        ..echo_tool()
    };
    let result = create_sandbox(vec![dashes, underscored, mcp])
        .execute(
            r#"
				try { ALL_TOOLS.push({}); } catch {}
				return {
					all: ALL_TOOLS,
					calls: [await tools.my_tool(), await tools["my-tool"](), await tools.mcp__docs__search()],
				};
			"#,
            ExecuteOptions::default(),
        )
        .await;
    assert_eq!(
        result.outcome.as_ref().unwrap().value,
        json!({
            "all": [
                {"name": "my_tool", "description": "Dashes"},
                {"name": "mcp__docs__search", "description": ""},
            ],
            "calls": ["dash", "dash", "mcp"],
        })
    );
}

// pi: sandbox.test.ts > tools > passes undefined arguments and results
// through.
#[tokio::test(flavor = "current_thread")]
async fn passes_undefined_arguments_and_results_through() {
    let noop = Tool {
        name: "noop".into(),
        execute: Arc::new(|args, _ctx| Box::pin(async move { Ok(args) })),
        ..echo_tool()
    };
    let result = create_sandbox(vec![noop])
        .execute(
            "return [await tools.noop(), await tools.noop(null)]",
            ExecuteOptions::default(),
        )
        .await;
    assert_eq!(result.outcome.as_ref().unwrap().value, json!([null, null]));
}

// pi: sandbox.test.ts > tools > turns tool errors into catchable Errors in
// the script.
#[tokio::test(flavor = "current_thread")]
async fn turns_tool_errors_into_catchable_errors() {
    let fail = Tool {
        name: "fail".into(),
        execute: Arc::new(|_args, _ctx| {
            Box::pin(async {
                Err(ToolError {
                    message: "tool exploded".into(),
                })
            })
        }),
        ..echo_tool()
    };
    let result = create_sandbox(vec![fail])
        .execute(
            r#"
				try {
					await tools.fail();
					return "no error";
				} catch (error) {
					return { isError: error instanceof Error, message: error.message };
				}
			"#,
            ExecuteOptions::default(),
        )
        .await;
    assert_eq!(
        result.outcome.as_ref().unwrap().value,
        json!({"isError": true, "message": "tool exploded"})
    );
    assert_eq!(result.calls.len(), 1);
    assert_eq!(result.calls[0].name, "fail");
    assert_eq!(result.calls[0].status, CallStatus::Error);
}

// pi: sandbox.test.ts > tools > rejects calls to unknown tools.
#[tokio::test(flavor = "current_thread")]
async fn rejects_calls_to_unknown_tools() {
    let result = create_sandbox(vec![])
        .execute("return await tools.missing()", ExecuteOptions::default())
        .await;
    let error = result.outcome.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Script);
    assert_eq!(error.name.as_deref(), Some("TypeError"));
}

// pi: sandbox.test.ts > tools > aborts unawaited calls when the script
// returns. The tool observes its abort signal; the call records as
// cancelled.
#[tokio::test(flavor = "current_thread")]
async fn aborts_unawaited_calls_when_the_script_returns() {
    let aborted = Arc::new(AtomicBool::new(false));
    let aborted2 = Arc::clone(&aborted);
    let slow = Tool {
        name: "slow".into(),
        execute: Arc::new(move |_args, ctx| {
            let aborted = Arc::clone(&aborted2);
            Box::pin(async move {
                ctx.signal.cancelled().await;
                aborted.store(true, Ordering::Relaxed);
                Err(ToolError {
                    message: "aborted".into(),
                })
            })
        }),
        ..echo_tool()
    };
    let result = create_sandbox(vec![slow])
        .execute("tools.slow(); return 'early'", ExecuteOptions::default())
        .await;
    assert_eq!(result.outcome.as_ref().unwrap().value, json!("early"));
    assert_eq!(result.calls.len(), 1);
    assert_eq!(result.calls[0].name, "slow");
    assert_eq!(result.calls[0].status, CallStatus::Cancelled);
    // The spawned tool task observes the cancellation on a later scheduler
    // pass; yield so it can record its abort.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert!(aborted.load(Ordering::Relaxed));
}

// pi: sandbox.test.ts > tools > names close matches when a script reads a
// tool that does not exist.
#[tokio::test(flavor = "current_thread")]
async fn names_close_matches_for_missing_tools() {
    let web_search = Tool {
        name: "web-search".into(),
        execute: Arc::new(|_a, _c| Box::pin(async { Ok(json!("")) })),
        ..echo_tool()
    };
    let sandbox = create_sandbox(vec![echo_tool(), web_search]);
    let attempt = async |expression: &str| {
        let script = format!("return {expression};");
        let result = sandbox.execute(&script, ExecuteOptions::default()).await;
        match result.outcome {
            Ok(success) => success.value.to_string(),
            Err(error) => error.message,
        }
    };
    assert_eq!(
        attempt("tools.Echo").await,
        "tools.Echo does not exist. Did you mean tools.echo? ALL_TOOLS lists every tool; searchTools(query) finds tools by topic. Check for a member with \"Echo\" in tools."
    );
    assert!(
        attempt("tools.websearch")
            .await
            .contains("Did you mean tools.web_search?")
    );
    assert!(
        attempt("tools.nothing")
            .await
            .contains("Available: echo, web_search.")
    );
    assert_eq!(
        attempt(
            "['echo' in tools, 'nothing' in tools, String(tools.toString), JSON.stringify(tools)]"
        )
        .await,
        json!([true, false, "undefined", "{}"]).to_string()
    );
}

// pi: sandbox.test.ts > tools > supports register and unregister between
// executions. Port adaptation: our SandboxOptions is fixed per engine, so the
// changed-tools behavior is exercised with two engines (the script-visible
// behavior for new runs is the same).
#[tokio::test(flavor = "current_thread")]
async fn changed_tools_between_executions() {
    let sandbox = create_sandbox(vec![echo_tool()]);
    let result = sandbox
        .execute("return await tools.echo('a')", ExecuteOptions::default())
        .await;
    assert_eq!(result.outcome.as_ref().unwrap().value, json!("a"));
    // A new engine without the tool: `echo` is no longer in tools.
    let sandbox2 = create_sandbox(vec![]);
    let result = sandbox2
        .execute("return 'echo' in tools", ExecuteOptions::default())
        .await;
    assert_eq!(result.outcome.as_ref().unwrap().value, json!(false));
}

// ------------------------------------------------- store and load (pi 423+) ---

// pi: sandbox.test.ts > store and load > reads the snapshot and reports
// writes.
#[tokio::test(flavor = "current_thread")]
async fn store_reads_snapshot_and_reports_writes() {
    let sandbox = create_sandbox(vec![]);
    let options = ExecuteOptions {
        store: BTreeMap::from([
            ("counter".to_owned(), json!(41)),
            ("old".to_owned(), json!("x")),
        ]),
        ..ExecuteOptions::default()
    };
    let result = sandbox
        .execute(
            r#"
				const seen = load("counter");
				store("counter", seen + 1);
				store("list", [1, { a: null }]);
				store("old", undefined);
				return [seen, load("counter"), load("missing"), load("old")];
			"#,
            options,
        )
        .await;
    assert_eq!(
        result.outcome.as_ref().unwrap().value,
        json!([41, 42, null, null])
    );
    let writes = &result.outcome.as_ref().unwrap().store_writes;
    assert_eq!(writes.set.get("counter"), Some(&json!(42)));
    assert_eq!(writes.set.get("list"), Some(&json!([1, {"a": null}])));
    assert_eq!(writes.delete, vec!["old".to_owned()]);
}

// pi: sandbox.test.ts > store and load > returns copies, so mutating a loaded
// value does not change the store.
#[tokio::test(flavor = "current_thread")]
async fn store_load_returns_copies() {
    let sandbox = create_sandbox(vec![]);
    let options = ExecuteOptions {
        store: BTreeMap::from([("obj".to_owned(), json!({"a": 1}))]),
        ..ExecuteOptions::default()
    };
    let result = sandbox
        .execute(
            r#"const value = load("obj"); value.a = 2; const kept = { b: 1 }; store("kept", kept); kept.b = 2;
				return [load("obj").a, load("kept").b];"#,
            options,
        )
        .await;
    assert_eq!(result.outcome.as_ref().unwrap().value, json!([1, 1]));
    let writes = &result.outcome.as_ref().unwrap().store_writes;
    assert_eq!(writes.set.get("kept"), Some(&json!({"b": 1})));
}

// pi: sandbox.test.ts > store and load > rejects invalid keys, values, and
// oversized writes inside the script.
#[tokio::test(flavor = "current_thread")]
async fn store_rejects_invalid_inside_script() {
    let sandbox = create_sandbox(vec![]);
    let result = sandbox
        .execute(
            r#"
				const attempt = (fn) => { try { fn(); return "ok"; } catch (error) { return error.name; } };
				return [
					attempt(() => store(1, "x")),
					attempt(() => load({})),
					attempt(() => store("fn", () => 1)),
					attempt(() => store("big", "x".repeat(300 * 1024))),
					attempt(() => { for (let i = 0; i < 8; i++) store("k" + i, "x".repeat(200 * 1024)); }),
				];
			"#,
            ExecuteOptions::default(),
        )
        .await;
    assert_eq!(
        result.outcome.as_ref().unwrap().value,
        json!([
            "TypeError",
            "TypeError",
            "TypeError",
            "RangeError",
            "RangeError"
        ])
    );
}

// pi: sandbox.test.ts > store and load > explains oversized writes.
#[tokio::test(flavor = "current_thread")]
async fn store_explains_oversized_writes() {
    let sandbox = create_sandbox(vec![]);
    let result = sandbox
        .execute(
            r#"store("img", "x".repeat(300 * 1024));"#,
            ExecuteOptions::default(),
        )
        .await;
    let error = result.outcome.unwrap_err();
    assert!(
        error
            .message
            .contains(r#"store("img") value has 307202 characters of JSON"#),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("Show images with image()"),
        "{}",
        error.message
    );
}

// pi: sandbox.test.ts > store and load > reserves the store and load names.
// pi throws at construction; our engine panics (D14: construction-time
// configuration error).
#[test]
#[should_panic(expected = "Invalid global name \"store\"")]
fn reserves_the_store_name() {
    let _ = create_sandbox_store_global("store");
}

#[test]
#[should_panic(expected = "Invalid global name \"load\"")]
fn reserves_the_load_name() {
    let _ = create_sandbox_store_global("load");
}

fn create_sandbox_store_global(name: &str) -> QuickJsEngine {
    QuickJsEngine::new(SandboxOptions {
        globals: vec![Tool {
            name: name.to_owned(),
            execute: Arc::new(|_args, _ctx| Box::pin(async { Ok(Value::Null) })),
            ..echo_tool()
        }],
        ..SandboxOptions::default()
    })
}
