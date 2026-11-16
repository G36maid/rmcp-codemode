//! MCP-layer naming, ingestion, and result conversion — a line-for-line Rust
//! mirror of pi's MCP surface: `packages/mcp/src/naming.ts`,
//! `packages/mcp/src/protocol/content.ts`, `packages/mcp/src/output-limit.ts`,
//! and the coding-agent extension's `tools.ts`/`resources.ts` (SPEC §8, D13,
//! OQ4). Engine-agnostic: serde types only, no wire dependency.
//!
//! Layering (SPEC §8.1): the MCP layer's names are **stricter** than the
//! engine layer's and pre-empt it — `mcp__<server>__<tool>` is already a
//! valid identifier, so `to_codemode_identifier` rarely fires for MCP tools.

use serde::Deserialize;
use serde_json::{Map, Value, json};

// ---------------------------------------------------------------- hashing --

/// Minimal in-tree SHA-256 (DEPENDENCY_POLICY: small primitives are
/// implemented directly, like the in-tree BM25 and `fnv1a`). Only the
/// 8-hex-character prefix is consumed, by [`canonical_tool_name`] (pi:
/// `createHash("sha256")…slice(0, 8)`, `naming.ts:26-28`).
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for block in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (index, word) in block.chunks_exact(4).enumerate() {
            w[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut digest = [0u8; 32];
    for (index, word) in h.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

fn sha256_hex_prefix(data: &[u8]) -> String {
    sha256(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("")
        .chars()
        .take(8)
        .collect()
}

/// Lenient standard-base64 decode: invalid characters are skipped (Node's
/// `Buffer.from(…, "base64")` behavior); a trailing group of 2 or 3 symbols
/// decodes its bytes and the rest is dropped.
fn base64_decode(input: &str) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let value = |byte: u8| -> Option<u32> {
        ALPHABET
            .iter()
            .position(|&candidate| candidate == byte)
            .map(|position| position as u32)
    };
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes() {
        let Some(digit) = value(byte) else {
            continue;
        };
        buffer = (buffer << 6) | digit;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    out
}

// ----------------------------------------------------------------- naming --

/// Provider tool names are limited to 64 characters of `[A-Za-z0-9_-]`. (pi:
/// `MAX_TOOL_NAME_LENGTH`, `naming.ts:11`.)
pub const MAX_TOOL_NAME_LENGTH: usize = 64;

/// `mcp__<server>__<tool>`, sanitized and shortened with a hash suffix when
/// too long. Like Codex, everything but `[A-Za-z0-9_]` becomes `_`, so the
/// name is also the identifier code mode scripts call it by. `is_taken`
/// reports names used by a different MCP tool: sanitizing can map two tools
/// to one name (`a-b` and `a_b`), which then get the hash suffix. (pi:
/// `createMcpToolName`, `naming.ts:18-30`.)
pub fn canonical_tool_name(server: &str, tool: &str, is_taken: impl Fn(&str) -> bool) -> String {
    let name = format!("mcp__{server}__{tool}")
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if name.len() <= MAX_TOOL_NAME_LENGTH && !is_taken(&name) {
        return name;
    }
    let hash = sha256_hex_prefix(format!("{server}\0{tool}").as_bytes());
    // JS `slice(0, 55)` clamps: a short-but-taken name keeps its full text.
    let keep = MAX_TOOL_NAME_LENGTH - hash.len() - 1;
    let head: String = name.chars().take(keep).collect();
    format!("{head}_{hash}")
}

/// Namespace of a server's tools: `mcp__<server>` with `-` replaced by `_`,
/// like the tool names. Server names that differ only in `-` and `_` share a
/// namespace, so a configuration must not have both. (pi: `mcpNamespace`,
/// `naming.ts:35-38`.)
pub fn mcp_namespace(server: &str) -> String {
    format!("mcp__{}", server.replace('-', "_"))
}

/// A server's one-line summary for the system prompt: the first line of its
/// configured `description`, else of the instructions it sent in
/// `initialize`; empty when it has neither. (pi: `mcpServerSummary`,
/// `naming.ts:41-44`.)
pub fn mcp_server_summary(description: Option<&str>, instructions: Option<&str>) -> String {
    let text = description
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .or(instructions.map(str::trim).filter(|text| !text.is_empty()))
        .unwrap_or("");
    text.split('\n').next().unwrap_or("").trim().to_string()
}

/// Config validation of a server name: only letters, digits, `_`, `-`
/// (SPEC §8.1; pi `docs/mcp.md:77`).
pub fn validate_server_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("server name must not be empty".into());
    }
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        Ok(())
    } else {
        Err(format!(
            "server name `{name}` may only contain letters, digits, `_`, and `-`"
        ))
    }
}

/// Names differing only in `-` vs `_` are **the same server** — a second
/// registration is rejected (SPEC §8.1).
pub fn servers_conflict(a: &str, b: &str) -> bool {
    a.replace('-', "_") == b.replace('-', "_")
}

/// Whether a tool name comes from MCP: a server tool (`mcp__…`) or one of the
/// three resource tools. (pi: `isMcpToolName`, `mcp-servers.ts:47-49`.)
pub fn is_mcp_tool_name(name: &str) -> bool {
    name.starts_with("mcp__")
        || matches!(
            name,
            "list_mcp_resources" | "list_mcp_resource_templates" | "read_mcp_resource"
        )
}

// -------------------------------------------------------------- ingestion --

/// The four boolean hints forwarded from a tool's `annotations`; `None` when
/// the tool has none. (pi: `ANNOTATION_HINTS`/`toToolAnnotations`,
/// `tools.ts:237-248`; icons and entry `_meta` are stripped.)
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolAnnotations {
    pub read_only: Option<bool>,
    pub destructive: Option<bool>,
    pub idempotent: Option<bool>,
    pub open_world: Option<bool>,
}

impl ToolAnnotations {
    fn from_wire(annotations: &Value) -> Option<Self> {
        let mut out = Self::default();
        let read = |key: &str| annotations.get(key).and_then(Value::as_bool);
        out.read_only = read("readOnlyHint");
        out.destructive = read("destructiveHint");
        out.idempotent = read("idempotentHint");
        out.open_world = read("openWorldHint");
        if out == Self::default() {
            None
        } else {
            Some(out)
        }
    }
}

/// A tool as a server offers it (`tools/list` item): the fields we forward.
/// Tolerant: servers may omit descriptions and schemas. Icons and entry
/// `_meta` are stripped at parse time (pi `resources.ts:53-55`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UpstreamTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Option<Value>,
    pub output_schema: Option<Value>,
    pub annotations: Option<ToolAnnotations>,
}

impl UpstreamTool {
    /// Parse one `tools/list` item, tolerantly. Tolerated: a missing
    /// `inputSchema`; a `title`/icons/`_meta` are ignored.
    pub fn from_wire(item: &Value) -> Option<Self> {
        let name = item.get("name")?.as_str()?.to_string();
        let annotations = item.get("annotations").and_then(ToolAnnotations::from_wire);
        Some(Self {
            name,
            description: item
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            input_schema: item.get("inputSchema").cloned(),
            output_schema: item.get("outputSchema").cloned(),
            annotations,
        })
    }
}

/// One ingested tool: the canonical `mcp__<server>__<tool>` name scripts and
/// search see, plus everything the catalog (§7) and the engine declarations
/// need. `description` defaults to `""` at the engine boundary (pi's
/// toolsJson).
#[derive(Debug, Clone, PartialEq)]
pub struct IngestedTool {
    pub canonical_name: String,
    pub original_name: String,
    pub description: Option<String>,
    pub input_schema: Option<Value>,
    pub output_schema: Option<Value>,
    pub annotations: Option<ToolAnnotations>,
}

/// Ingest one server's tool list. Like Codex, **all** tools whose sanitized
/// names collide get the hash suffix, so which one would keep the plain name
/// does not depend on the order of the list (pi `index.ts:396-418`:
/// `plain` duplicates make every candidate taken). A `sha256` suffix
/// disambiguates; two colliding tools keep distinct canonical names.
pub fn ingest_tools(server: &str, tools: &[UpstreamTool]) -> Vec<IngestedTool> {
    // pi: `[...new Set(connection.tools.map((tool) => tool.name))].map(…)` —
    // the plain (unsuffixed) sanitized name of every distinct ORIGINAL tool
    // name; sanitizing may collapse distinct names, which is the collision
    // signal.
    let mut seen_names = std::collections::HashSet::new();
    let mut plain: Vec<String> = Vec::new();
    for tool in tools {
        if seen_names.insert(tool.name.as_str()) {
            plain.push(canonical_tool_name(server, &tool.name, |_| false));
        }
    }
    let mut owners: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut current: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    tools
        .iter()
        .map(|tool| {
            let owner = format!("{server}\0{}", tool.name);
            let name = canonical_tool_name(server, &tool.name, |candidate| {
                owners
                    .get(candidate)
                    .is_some_and(|existing| existing != &owner)
                    || current.contains(candidate)
                    || plain.iter().filter(|plain| **plain == candidate).count() > 1
            });
            owners.insert(name.clone(), owner);
            current.insert(name.clone());
            IngestedTool {
                canonical_name: name,
                original_name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
                output_schema: tool.output_schema.clone(),
                annotations: tool.annotations.clone(),
            }
        })
        .collect()
}

// ------------------------------------------------------ result conversion --

/// One MCP content block, tolerantly parsed. (pi: `ContentBlock` union —
/// text, image, audio, resource_link, embedded resource with `text` or
/// `blob`.)
#[derive(Debug, Clone, PartialEq)]
pub enum McpContentBlock {
    Text {
        text: String,
    },
    Image {
        data: String,
        mime_type: String,
    },
    Audio {
        mime_type: String,
    },
    ResourceLink {
        uri: String,
        name: String,
        title: Option<String>,
        description: Option<String>,
        mime_type: Option<String>,
        size: Option<u64>,
    },
    EmbeddedText {
        uri: String,
        text: String,
    },
    EmbeddedImage {
        uri: String,
        data: String,
        mime_type: String,
    },
    EmbeddedBlob {
        uri: String,
        blob: String,
        mime_type: Option<String>,
    },
    Unsupported {
        kind: String,
    },
}

impl McpContentBlock {
    /// Parse a wire content block. An unknown `type` becomes
    /// [`McpContentBlock::Unsupported`] so the result still converts.
    pub fn from_wire(block: &Value) -> Option<Self> {
        let kind = block.get("type")?.as_str()?.to_string();
        let string = |key: &str| block.get(key).and_then(Value::as_str);
        Some(match kind.as_str() {
            "text" => Self::Text {
                text: string("text")?.to_string(),
            },
            "image" => Self::Image {
                data: string("data")?.to_string(),
                mime_type: string("mimeType")?.to_string(),
            },
            "audio" => Self::Audio {
                mime_type: string("mimeType")?.to_string(),
            },
            "resource_link" => Self::ResourceLink {
                uri: string("uri")?.to_string(),
                name: string("name").unwrap_or_default().to_string(),
                title: string("title").map(str::to_string),
                description: string("description").map(str::to_string),
                mime_type: string("mimeType").map(str::to_string),
                size: block.get("size").and_then(Value::as_u64),
            },
            "resource" => {
                let resource = block.get("resource")?;
                let uri = resource.get("uri")?.as_str()?.to_string();
                if let Some(text) = resource.get("text").and_then(Value::as_str) {
                    Self::EmbeddedText {
                        uri,
                        text: text.to_string(),
                    }
                } else {
                    let data = resource.get("blob")?.as_str()?.to_string();
                    let mime_type = resource
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    if mime_type
                        .as_deref()
                        .is_some_and(|mime| mime.starts_with("image/"))
                    {
                        Self::EmbeddedImage {
                            uri,
                            data,
                            mime_type: mime_type.unwrap_or_default(),
                        }
                    } else {
                        Self::EmbeddedBlob {
                            uri,
                            blob: data,
                            mime_type,
                        }
                    }
                }
            }
            other => Self::Unsupported {
                kind: other.to_string(),
            },
        })
    }

    fn to_wire(&self) -> Value {
        match self {
            Self::Text { text } => json!({ "type": "text", "text": text }),
            Self::Image { data, mime_type } => {
                json!({ "type": "image", "data": data, "mimeType": mime_type })
            }
            Self::Audio { mime_type } => json!({ "type": "audio", "mimeType": mime_type }),
            Self::ResourceLink {
                uri,
                name,
                title,
                description,
                mime_type,
                size,
            } => {
                let mut object = Map::new();
                object.insert("type".into(), json!("resource_link"));
                object.insert("uri".into(), json!(uri));
                object.insert("name".into(), json!(name));
                if let Some(title) = title {
                    object.insert("title".into(), json!(title));
                }
                if let Some(description) = description {
                    object.insert("description".into(), json!(description));
                }
                if let Some(mime_type) = mime_type {
                    object.insert("mimeType".into(), json!(mime_type));
                }
                if let Some(size) = size {
                    object.insert("size".into(), json!(size));
                }
                Value::Object(object)
            }
            Self::EmbeddedText { uri, text } => {
                json!({ "type": "resource", "resource": { "uri": uri, "text": text } })
            }
            Self::EmbeddedImage {
                uri,
                data,
                mime_type,
            } => {
                json!({ "type": "resource", "resource": { "uri": uri, "blob": data, "mimeType": mime_type } })
            }
            Self::EmbeddedBlob {
                uri,
                blob,
                mime_type,
            } => {
                let mut resource = Map::new();
                resource.insert("uri".into(), json!(uri));
                resource.insert("blob".into(), json!(blob));
                if let Some(mime_type) = mime_type {
                    resource.insert("mimeType".into(), json!(mime_type));
                }
                json!({ "type": "resource", "resource": resource })
            }
            Self::Unsupported { kind } => json!({ "type": kind }),
        }
    }
}

/// Model-facing content: text and image blocks. (pi: `LlmContent`.)
#[derive(Debug, Clone, PartialEq)]
pub enum ModelContent {
    Text { text: String },
    Image { data: String, mime_type: String },
}

/// A `CallToolResult` as the server sent it. `_meta` is carried separately
/// only to be dropped: it never reaches scripts (pi `tools.ts:228`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallToolResult {
    pub content: Vec<McpContentBlock>,
    pub structured_content: Option<Value>,
    pub is_error: bool,
}

/// The converted result. `script_result` is what a codemode script resolves
/// to — the full `CallToolResult` minus `_meta` — while `content`/`is_error`
/// describe the model-facing side (`isError: true` marks output only; the
/// script still resolves). `full_output_path` is set when the output was
/// truncated and saved.
#[derive(Debug, Clone, PartialEq)]
pub struct ConvertedResult {
    pub content: Vec<ModelContent>,
    pub script_result: Value,
    pub is_error: bool,
    pub full_output_path: Option<String>,
}

/// Blobs of these types are shown as text. (pi: `isTextMimeType` — the type
/// before `;` parameters, lowercased.)
fn is_text_mime_type(mime_type: Option<&str>) -> bool {
    let Some(mime_type) = mime_type else {
        return false;
    };
    mime_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase()
        .starts_with("text/")
}

/// `{n}B` / `{n.n}KB` / `{n.n}MB`. (pi: `formatSize`, `truncate.ts:61-69`.)
fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// File extension of a URI for the saved-full-output file. (pi:
/// `extensionOf`, `tools.ts:160-163`.)
fn extension_of(uri: &str) -> String {
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    match path.rsplit('/').next() {
        Some(file) if file.contains('.') => match file.rsplit('.').next() {
            Some(extension)
                if !extension.is_empty()
                    && extension.len() <= 8
                    && extension.chars().all(|c| c.is_ascii_alphanumeric()) =>
            {
                format!(".{extension}")
            }
            _ => ".bin".into(),
        },
        _ => ".bin".into(),
    }
}

/// Where saved output goes. (pi: `McpOutputSaver` — the embedder decides
/// where full output lands; the crate only reports the path.)
pub type OutputSaver<'a> = dyn FnMut(&str, &str) -> Result<String, String> + 'a;

/// pi's `blockToLlmContent` (protocol/content.ts:79-101): the per-block
/// fallback conversion.
fn block_to_llm_content(block: &McpContentBlock) -> ModelContent {
    match block {
        McpContentBlock::Text { text } => ModelContent::Text { text: text.clone() },
        McpContentBlock::Image { data, mime_type } => ModelContent::Image {
            data: data.clone(),
            mime_type: mime_type.clone(),
        },
        McpContentBlock::Audio { mime_type } => ModelContent::Text {
            text: format!("[audio {mime_type} omitted]"),
        },
        McpContentBlock::ResourceLink { name, uri, .. } => ModelContent::Text {
            text: format!("{name}: {uri}"),
        },
        McpContentBlock::EmbeddedText { text, .. } => ModelContent::Text { text: text.clone() },
        McpContentBlock::EmbeddedImage {
            data, mime_type, ..
        } => ModelContent::Image {
            data: data.clone(),
            mime_type: mime_type.clone(),
        },
        McpContentBlock::EmbeddedBlob { uri, mime_type, .. } => ModelContent::Text {
            text: format!(
                "[binary resource {uri} ({}) omitted]",
                mime_type.as_deref().unwrap_or("unknown type")
            ),
        },
        McpContentBlock::Unsupported { kind } => ModelContent::Text {
            text: format!("[unsupported MCP content {kind}]"),
        },
    }
}

/// pi's `blockToContent` (tools.ts:173-202): the richer conversion used by
/// [`convert_mcp_result`] — resource links and binary embedded resources get
/// informative placeholders, and binary text-like resources decode.
fn block_to_content(
    server: &str,
    block: &McpContentBlock,
    readable_resources: bool,
    saver: Option<&mut OutputSaver<'_>>,
) -> Vec<ModelContent> {
    if let McpContentBlock::ResourceLink {
        uri,
        name,
        title,
        description,
        mime_type,
        size,
    } = block
    {
        let mut details = Vec::new();
        if let Some(mime_type) = mime_type {
            details.push(mime_type.clone());
        }
        if let Some(size) = size {
            details.push(format_size(*size));
        }
        let read = if readable_resources {
            format!(". Read it with read_mcp_resource (server \"{server}\")")
        } else {
            String::new()
        };
        let description = description
            .as_deref()
            .map(|description| format!(": {description}"))
            .unwrap_or_default();
        let details_text = if details.is_empty() {
            String::new()
        } else {
            format!(" ({})", details.join(", "))
        };
        let title = title.clone().unwrap_or_else(|| name.clone());
        return vec![ModelContent::Text {
            text: format!("[Resource {uri} \"{title}\"{details_text}{description}{read}]"),
        }];
    }
    if let McpContentBlock::EmbeddedBlob {
        uri,
        blob,
        mime_type,
    } = block
        && !mime_type
            .as_deref()
            .is_some_and(|mime_type| mime_type.starts_with("image/"))
    {
        let data = base64_decode(blob);
        if is_text_mime_type(mime_type.as_deref()) {
            return vec![ModelContent::Text {
                text: String::from_utf8_lossy(&data).into_owned(),
            }];
        }
        let kind = format!(
            "{}, {}",
            mime_type.as_deref().unwrap_or("unknown type"),
            format_size(data.len() as u64)
        );
        let fall_back = |reason: String| -> Vec<ModelContent> {
            vec![ModelContent::Text {
                text: format!("[Binary resource {uri} ({kind}) could not be saved: {reason}]"),
            }]
        };
        let Some(saver) = saver else {
            return fall_back("no output saver".into());
        };
        let path = saver(&String::from_utf8_lossy(&data), &extension_of(uri));
        return match path {
            Ok(path) => vec![ModelContent::Text {
                text: format!("[Binary resource {uri} ({kind}) saved to {path}]"),
            }],
            Err(reason) => fall_back(reason),
        };
    }
    vec![block_to_llm_content(block)]
}

fn text_of(content: &[ModelContent]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ModelContent::Text { text } => Some(text.as_str()),
            ModelContent::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Model-facing text of an MCP result beyond this is cut in the middle. (pi:
/// `MCP_OUTPUT_MAX_BYTES`, `output-limit.ts:10`.)
pub const MCP_OUTPUT_MAX_BYTES: usize = 20 * 1024;

/// Keep the start and the end of `text`, half of `max_bytes` each, and
/// replace the middle with a `…N chars truncated…` marker. Cuts only at
/// character boundaries. `None` when `text` fits. (pi: `truncateMiddle`,
/// `output-limit.ts:88-105`.)
fn truncate_middle(text: &str, max_bytes: usize) -> Option<(String, usize, usize)> {
    let buffer = text.as_bytes();
    if buffer.len() <= max_bytes {
        return None;
    }
    // Continuation bytes (10xxxxxx) are not character starts.
    let is_boundary = |index: usize| index >= buffer.len() || (buffer[index] & 0xc0) != 0x80;
    let mut head_end = max_bytes / 2;
    while head_end > 0 && !is_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = buffer.len() - (max_bytes - max_bytes / 2);
    while tail_start < buffer.len() && !is_boundary(tail_start) {
        tail_start += 1;
    }
    let head = String::from_utf8_lossy(&buffer[..head_end]);
    let tail = String::from_utf8_lossy(&buffer[tail_start..]);
    let removed_chars = String::from_utf8_lossy(&buffer[head_end..tail_start])
        .chars()
        .count();
    // A final newline ends the last line instead of starting another.
    let lines = text.split('\n').count() - usize::from(text.ends_with('\n'));
    Some((
        format!("{head}…{removed_chars} chars truncated…{tail}"),
        buffer.len(),
        lines,
    ))
}

/// Keep model-facing text within [`MCP_OUTPUT_MAX_BYTES`]. Longer text
/// becomes one text block in Codex's truncation format, followed by the path
/// of the file with the full text when a saver is given; images follow it.
/// Content within the limit is returned as it is. (pi: `limitMcpContent`,
/// `output-limit.ts:28-52`.)
fn limit_mcp_content(
    content: Vec<ModelContent>,
    mut saver: Option<&mut OutputSaver<'_>>,
) -> (Vec<ModelContent>, Option<String>) {
    let combined = content
        .iter()
        .filter_map(|block| match block {
            ModelContent::Text { text } => Some(text.as_str()),
            ModelContent::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let Some((truncated, total_bytes, total_lines)) =
        truncate_middle(&combined, MCP_OUTPUT_MAX_BYTES)
    else {
        return (content, None);
    };
    let mut full_output_path: Option<String> = None;
    let mut where_note = String::new();
    if let Some(saver) = saver.as_mut() {
        match saver(&combined, ".txt") {
            Ok(path) => {
                where_note = format!("\n\n[Full output: {path} (read it with offset/limit)]");
                full_output_path = Some(path);
            }
            Err(reason) => {
                where_note = format!("\n\n[Could not save the full output: {reason}]");
            }
        }
    }
    let tokens = total_bytes.div_ceil(4);
    let text = format!(
        "Warning: truncated output (original token count: {tokens})\nTotal output lines: {total_lines}\n\n{truncated}{where_note}"
    );
    let mut out = vec![ModelContent::Text { text }];
    out.extend(
        content
            .into_iter()
            .filter(|block| matches!(block, ModelContent::Image { .. })),
    );
    (out, full_output_path)
}

/// Convert an MCP result. `isError` results become error results that keep
/// the structured result — **scripts receive the full result; only the
/// model-facing output is marked** (pi `convertMcpResult`,
/// `tools.ts:215-235`; `tools.ts:8-10`). `readable_resources` switches the
/// resource-link read hint; the saver receives full output when it truncates.
pub fn convert_mcp_result(
    server: &str,
    tool: &str,
    result: &CallToolResult,
    readable_resources: bool,
    saver: Option<&mut OutputSaver<'_>>,
) -> ConvertedResult {
    // Without content blocks, the structured content becomes its JSON. (pi
    // `toLlmContent`, protocol/content.ts:112-118.)
    let mut saver = saver;
    let mut converted: Vec<ModelContent> = if !result.content.is_empty() {
        result
            .content
            .iter()
            .flat_map(|block| {
                block_to_content(server, block, readable_resources, saver.as_deref_mut())
            })
            .collect()
    } else if let Some(structured) = &result.structured_content {
        vec![ModelContent::Text {
            text: serde_json::to_string_pretty(structured).unwrap_or_else(|_| "null".into()),
        }]
    } else {
        Vec::new()
    };
    if result.is_error && text_of(&converted).is_empty() {
        converted.push(ModelContent::Text {
            text: format!("MCP tool {server}/{tool} returned an error"),
        });
    }
    let (content, full_output_path) = limit_mcp_content(converted, saver);
    // The script's result: the whole CallToolResult minus `_meta` (dropped at
    // parse time — see [`CallToolResult`]).
    let mut script_blocks = Vec::with_capacity(result.content.len());
    for block in &result.content {
        script_blocks.push(block.to_wire());
    }
    let mut script = Map::new();
    script.insert("content".into(), Value::Array(script_blocks));
    if let Some(structured) = &result.structured_content {
        script.insert("structuredContent".into(), structured.clone());
    }
    if result.is_error {
        script.insert("isError".into(), Value::Bool(true));
    }
    ConvertedResult {
        content,
        script_result: Value::Object(script),
        is_error: result.is_error,
        full_output_path,
    }
}

/// Parse a wire `tools/list` result into upstream tools. Tolerated: missing
/// capability entries upstream produce empty lists at the caller.
pub fn parse_tool_list(wire: &Value) -> Vec<UpstreamTool> {
    wire.get("tools")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(UpstreamTool::from_wire).collect())
        .unwrap_or_default()
}

/// A `tools/list` item straight from the wire, for hosts that hold raw JSON.
pub fn tool_from_wire(item: &Value) -> Option<UpstreamTool> {
    UpstreamTool::from_wire(item)
}

/// Wire-parse a `CallToolResult` tolerantly: unknown block types become
/// [`McpContentBlock::Unsupported`]; `_meta` is read only to be dropped.
pub fn call_tool_result_from_wire(wire: &Value) -> CallToolResult {
    CallToolResult {
        content: wire
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(McpContentBlock::from_wire)
                    .collect()
            })
            .unwrap_or_default(),
        structured_content: wire.get("structuredContent").cloned(),
        is_error: wire
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// Payload shape documented for the tolerance checks: a wire `tools/list`
/// item. (Kept here so the §10 layer and the tests agree on the shape.)
#[derive(Debug, Deserialize)]
pub struct WireToolList {
    pub tools: Vec<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(sha256_hex_prefix(b""), "e3b0c442");
        assert_eq!(sha256_hex_prefix(b"abc"), "ba7816bf");
        assert_eq!(
            &sha256(b"abc")
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // The long-vector test (RFC 6234): one million 'a' bytes, first bytes
        // cdc76e5c…
        let long = vec![b'a'; 1_000_000];
        assert_eq!(sha256_hex_prefix(&long), "cdc76e5c");
    }

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_decode(""), b"");
        assert_eq!(base64_decode("Zm9v"), b"foo");
        assert_eq!(base64_decode("Zm9vYmFy"), b"foobar");
        assert_eq!(base64_decode("Zm9vYmE="), b"fooba");
        // Invalid characters are skipped (Node Buffer semantics).
        assert_eq!(base64_decode("Zm 9v\nYmFy"), b"foobar");
    }
}
