//! Model Context Protocol server over stdio (`bugle mcp`).
//!
//! Implements the subset agents need: `initialize`, `ping`, `tools/list`,
//! `tools/call`. Newline-delimited JSON-RPC 2.0 on stdin/stdout; logs go to
//! stderr so they never corrupt the stream.

use crate::actions::{self, catalog};
use base64::Engine as _;
use bugle_engine::Engine;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub const PROTOCOL_VERSION: &str = "2024-11-05";

pub async fn serve(engine: Arc<Engine>) -> anyhow::Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut lines = stdin.lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write(&mut stdout, &error_response(Value::Null, -32700, &format!("parse error: {e}"))).await?;
                continue;
            }
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        if method.starts_with("notifications/") {
            continue;
        }
        let response = match method.as_str() {
            "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "bugle", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Bugle takes screenshots and recordings of this Mac and runs OCR/AI actions on them. Call `capture` to get an image back; use the returned media id with ocr/describe/annotate. Coordinates for targets are screen points; annotation coordinates are image pixels."
            }}),
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": catalog().iter().map(|t| json!({
                "name": t.name, "description": t.description, "inputSchema": t.input_schema
            })).collect::<Vec<_>>()}}),
            "tools/call" => {
                let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                match actions::run(&engine, name, args).await {
                    Ok(out) => {
                        let mut content = vec![json!({"type": "text", "text": serde_json::to_string_pretty(&out.json)?})];
                        if let Some(img) = out.image {
                            content.push(json!({"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(img), "mimeType": "image/png"}));
                        }
                        json!({"jsonrpc": "2.0", "id": id, "result": {"content": content, "structuredContent": out.json, "isError": false}})
                    }
                    Err(e) => json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": format!("error: {e:#}")}], "isError": true}}),
                }
            }
            _ => error_response(id, -32601, &format!("method not found: {method}")),
        };
        write(&mut stdout, &response).await?;
    }
    Ok(())
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

async fn write(out: &mut tokio::io::Stdout, v: &Value) -> anyhow::Result<()> {
    let mut s = serde_json::to_string(v)?;
    s.push('\n');
    out.write_all(s.as_bytes()).await?;
    out.flush().await?;
    Ok(())
}
