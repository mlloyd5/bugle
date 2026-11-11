//! Model Context Protocol server over stdio (`bugle mcp`).
//!
//! Implements the subset agents need: `initialize`, `ping`, `tools/list`,
//! `tools/call`. Newline-delimited JSON-RPC 2.0 on stdin/stdout; logs go to
//! stderr so they never corrupt the stream.
//!
//! This is an `initialize`-handshake server. It speaks the protocol
//! revisions in [`SUPPORTED_PROTOCOL_VERSIONS`]: the client's requested
//! version is echoed when it is one of them, otherwise the latest is offered
//! and the client decides whether to continue.
//!
//! Every request is handled on its own task, so a slow `tools/call` (a long
//! recording, a model call) does not hold up `ping` or other requests;
//! responses go through a single writer so lines never interleave.
//! Notifications (messages without an `id`) are never answered. Batches
//! (JSON arrays) are accepted as in plain JSON-RPC 2.0.

use crate::actions::{self, catalog};
use base64::Engine as _;
use bugle_engine::Engine;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The latest protocol revision this server speaks.
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// Every protocol revision this server speaks, newest first.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

pub async fn serve(engine: Arc<Engine>) -> anyhow::Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(v) = rx.recv().await {
            write(&mut stdout, &v).await?;
        }
        anyhow::Ok(())
    });
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let engine = engine.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Some(response) = handle_line(&engine, &line).await {
                let _ = tx.send(response);
            }
        });
    }
    // stdin closed: let in-flight requests finish, then stop the writer.
    drop(tx);
    writer.await??;
    Ok(())
}

/// Parse one line of input and handle it. `None` means nothing is written.
pub async fn handle_line(engine: &Arc<Engine>, line: &str) -> Option<Value> {
    match serde_json::from_str::<Value>(line) {
        Ok(msg) => handle_message(engine, msg).await,
        Err(e) => Some(error_response(Value::Null, PARSE_ERROR, &format!("parse error: {e}"))),
    }
}

/// Handle one decoded JSON-RPC message (a single message or a batch).
/// Returns the response to write, or `None` for notifications, client
/// responses and batches made only of those.
pub async fn handle_message(engine: &Arc<Engine>, msg: Value) -> Option<Value> {
    match msg {
        Value::Array(items) => {
            if items.is_empty() {
                return Some(error_response(Value::Null, INVALID_REQUEST, "invalid request: empty batch"));
            }
            let mut set = tokio::task::JoinSet::new();
            for (i, item) in items.into_iter().enumerate() {
                let engine = engine.clone();
                set.spawn(async move { (i, handle_single(&engine, item).await) });
            }
            let mut out: Vec<(usize, Value)> = vec![];
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok((i, Some(v))) => out.push((i, v)),
                    Ok((_, None)) => {}
                    Err(e) => out.push((usize::MAX, error_response(Value::Null, -32603, &format!("internal error: {e}")))),
                }
            }
            out.sort_by_key(|(i, _)| *i);
            (!out.is_empty()).then(|| Value::Array(out.into_iter().map(|(_, v)| v).collect()))
        }
        other => handle_single(engine, other).await,
    }
}

async fn handle_single(engine: &Arc<Engine>, msg: Value) -> Option<Value> {
    let Value::Object(obj) = msg else {
        return Some(error_response(Value::Null, INVALID_REQUEST, "invalid request: expected a JSON object"));
    };
    let id = obj.get("id").cloned();
    let method = obj.get("method").and_then(|m| m.as_str());

    let Some(method) = method else {
        // A response from the client (we never send requests) is ignored.
        if obj.contains_key("result") || obj.contains_key("error") {
            return None;
        }
        return Some(error_response(valid_id_or_null(&id), INVALID_REQUEST, "invalid request: missing method"));
    };

    // No `id` member at all: a notification, which is never answered, even
    // when the method is unknown.
    let id = id?;
    if !(id.is_string() || id.is_i64() || id.is_u64()) {
        return Some(error_response(valid_id_or_null(&Some(id)), INVALID_REQUEST, "invalid request: id must be a string or integer"));
    }
    if obj.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return Some(error_response(id, INVALID_REQUEST, "invalid request: jsonrpc must be \"2.0\""));
    }
    let params = match obj.get("params") {
        None | Some(Value::Null) => json!({}),
        Some(p @ Value::Object(_)) => p.clone(),
        Some(_) => return Some(error_response(id, INVALID_PARAMS, "invalid params: expected an object")),
    };

    Some(match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(|v| v.as_str());
            let version = requested.filter(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(v)).unwrap_or(PROTOCOL_VERSION);
            result_response(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "bugle", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "Bugle takes screenshots and recordings of this Mac and runs OCR/AI actions on them. Call `capture` to get an image back; use the returned media id with ocr/describe/annotate. Coordinates for targets are screen points; annotation coordinates are image pixels."
                }),
            )
        }
        "ping" => result_response(id, json!({})),
        "tools/list" => result_response(
            id,
            json!({"tools": catalog().iter().map(|t| json!({
                "name": t.name, "description": t.description, "inputSchema": t.input_schema
            })).collect::<Vec<_>>()}),
        ),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(|n| n.as_str()) else {
                return Some(error_response(id, INVALID_PARAMS, "invalid params: tools/call needs a string `name`"));
            };
            if !catalog().iter().any(|t| t.name == name) {
                return Some(error_response(id, INVALID_PARAMS, &format!("unknown tool: {name}")));
            }
            let args = match params.get("arguments") {
                None | Some(Value::Null) => json!({}),
                Some(a @ Value::Object(_)) => a.clone(),
                Some(_) => return Some(error_response(id, INVALID_PARAMS, "invalid params: `arguments` must be an object")),
            };
            match actions::run(engine, name, args).await {
                Ok(out) => {
                    // Compact text: `structuredContent` already carries the
                    // same JSON for clients that read it.
                    let text = serde_json::to_string(&out.json).unwrap_or_default();
                    let mut content = vec![json!({"type": "text", "text": text})];
                    if let Some(img) = out.image {
                        content.push(json!({"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(img), "mimeType": "image/png"}));
                    }
                    result_response(id, json!({"content": content, "structuredContent": out.json, "isError": false}))
                }
                Err(e) => result_response(id, json!({"content": [{"type": "text", "text": format!("error: {e:#}")}], "isError": true})),
            }
        }
        _ => error_response(id, METHOD_NOT_FOUND, &format!("method not found: {method}")),
    })
}

/// The request id to echo in an error, or null when it is not a valid id.
fn valid_id_or_null(id: &Option<Value>) -> Value {
    match id {
        Some(v) if v.is_string() || v.is_i64() || v.is_u64() => v.clone(),
        _ => Value::Null,
    }
}

fn result_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
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

#[cfg(test)]
mod tests {
    use super::*;
    use bugle_core::config::Config;

    fn engine(tmp: &std::path::Path) -> Arc<Engine> {
        let mut cfg = Config::default();
        cfg.general.save_dir = tmp.join("save").display().to_string();
        cfg.general.history_path = Some(tmp.join("history.jsonl").display().to_string());
        Arc::new(Engine::new(cfg, false).unwrap())
    }

    #[tokio::test]
    async fn notifications_are_never_answered() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine(tmp.path());
        for msg in [
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "method": "ping"}),
            json!({"jsonrpc": "2.0", "method": "no/such/method", "params": {}}),
            json!({"jsonrpc": "2.0", "method": "tools/call", "params": {"name": "nope"}}),
            // A response to a server request is not answered either.
            json!({"jsonrpc": "2.0", "id": 7, "result": {}}),
        ] {
            assert_eq!(handle_message(&e, msg.clone()).await, None, "{msg}");
        }
        // An id-carrying ping is a request and gets a result.
        let r = handle_message(&e, json!({"jsonrpc": "2.0", "id": "a", "method": "ping"})).await.unwrap();
        assert_eq!(r, json!({"jsonrpc": "2.0", "id": "a", "result": {}}));
    }

    #[tokio::test]
    async fn version_negotiation() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine(tmp.path());
        let init = |v: Value| json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": v, "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}});
        for v in SUPPORTED_PROTOCOL_VERSIONS {
            let r = handle_message(&e, init(json!(v))).await.unwrap();
            assert_eq!(r["result"]["protocolVersion"], *v);
        }
        let r = handle_message(&e, init(json!("1999-01-01"))).await.unwrap();
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSION);
        let r = handle_message(&e, json!({"jsonrpc": "2.0", "id": 2, "method": "initialize"})).await.unwrap();
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(r["id"], 2);
        assert_eq!(r["result"]["serverInfo"]["name"], "bugle");
    }

    #[tokio::test]
    async fn unknown_method_and_invalid_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine(tmp.path());
        let r = handle_message(&e, json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"})).await.unwrap();
        assert_eq!(r["id"], 3);
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);

        for bad in [json!(5), json!("x"), json!(null)] {
            let r = handle_message(&e, bad).await.unwrap();
            assert_eq!(r["error"]["code"], INVALID_REQUEST);
            assert!(r["id"].is_null());
        }
        let r = handle_message(&e, json!({"jsonrpc": "2.0", "id": null, "method": "ping"})).await.unwrap();
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
        let r = handle_line(&e, "{not json").await.unwrap();
        assert_eq!(r["error"]["code"], PARSE_ERROR);
    }

    #[tokio::test]
    async fn unknown_tool_is_invalid_params() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine(tmp.path());
        let r = handle_message(&e, json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "nope", "arguments": {}}})).await.unwrap();
        assert_eq!(r["id"], 4);
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        assert!(r["error"]["message"].as_str().unwrap().contains("nope"));
        let r = handle_message(&e, json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {}})).await.unwrap();
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
    }

    #[tokio::test]
    async fn tool_result_text_is_compact() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine(tmp.path());
        // `providers` needs no permissions and no network.
        let r = handle_message(&e, json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "providers"}})).await.unwrap();
        let result = &r["result"];
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(!text.contains('\n'));
        assert_eq!(serde_json::from_str::<Value>(text).unwrap(), result["structuredContent"]);
    }

    #[tokio::test]
    async fn batch() {
        let tmp = tempfile::tempdir().unwrap();
        let e = engine(tmp.path());
        let r = handle_message(
            &e,
            json!([
                {"jsonrpc": "2.0", "id": 1, "method": "ping"},
                {"jsonrpc": "2.0", "method": "notifications/initialized"},
                {"jsonrpc": "2.0", "id": 2, "method": "nope"},
                7
            ]),
        )
        .await
        .unwrap();
        let arr = r.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0], json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
        assert_eq!(arr[1]["id"], 2);
        assert_eq!(arr[1]["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(arr[2]["error"]["code"], INVALID_REQUEST);

        // Only notifications: nothing to write.
        assert_eq!(handle_message(&e, json!([{"jsonrpc": "2.0", "method": "notifications/initialized"}])).await, None);
        // Empty batch is a single invalid-request error.
        let r = handle_message(&e, json!([])).await.unwrap();
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
    }
}
