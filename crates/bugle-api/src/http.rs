//! JSON HTTP API (`bugle serve`).
//!
//! Every action is reachable as `POST /v1/actions/{name}`; the REST-style
//! aliases below exist for readability. Responses are
//! `{ "ok": true, ...result, "image_base64"?: "..." }` or
//! `{ "ok": false, "error": "..." }`.

use crate::actions::{self, catalog, status_for};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::Engine as _;
use bugle_engine::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    pub token: Option<String>,
}

pub fn router(engine: Arc<Engine>, token: Option<String>, cors: bool) -> Router {
    let state = AppState { engine, token };
    let mut r = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/openapi.json", get(openapi))
        .route("/v1/tools", get(tools))
        .route("/v1/actions/{name}", post(action_post))
        .route("/v1/capture", post(|s, j| action_post(s, Path("capture".into()), j)))
        .route("/v1/scroll", post(|s, j| action_post(s, Path("scroll_capture".into()), j)))
        .route("/v1/ocr", post(|s, j| action_post(s, Path("ocr".into()), j)))
        .route("/v1/describe", post(|s, j| action_post(s, Path("describe".into()), j)))
        .route("/v1/annotate", post(|s, j| action_post(s, Path("annotate".into()), j)))
        .route("/v1/annotate/schema", get(|| async { Json(bugle_markup::schema_description()) }))
        .route("/v1/transcribe", post(|s, j| action_post(s, Path("transcribe".into()), j)))
        .route("/v1/subtitles", post(|s, j| action_post(s, Path("subtitles".into()), j)))
        .route("/v1/complete", post(|s, j| action_post(s, Path("complete".into()), j)))
        .route("/v1/record/start", post(|s, j| action_post(s, Path("record_start".into()), j)))
        .route("/v1/record/stop", post(|s| action_post(s, Path("record_stop".into()), Json(json!({})))))
        .route("/v1/record/status", get(|s| action_get(s, "record_status")))
        .route("/v1/pin", post(|s, j| action_post(s, Path("pin".into()), j)))
        .route("/v1/copy", post(|s, j| action_post(s, Path("copy".into()), j)))
        .route("/v1/select", post(|s| action_post(s, Path("select_area".into()), Json(json!({})))))
        .route("/v1/displays", get(|s| action_get(s, "list_displays")))
        .route("/v1/windows", get(windows))
        .route("/v1/providers", get(|s| action_get(s, "providers")))
        .route("/v1/permissions", get(|s| action_get(s, "permissions")))
        .route("/v1/history", get(history))
        .route("/v1/media/{id}", get(media_get).delete(media_delete))
        .route("/v1/media/{id}/file", get(media_file))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, auth));
    if cors {
        r = r.layer(tower_http::cors::CorsLayer::very_permissive());
    }
    r
}

pub async fn serve(engine: Arc<Engine>, bind: &str, token: Option<String>, cors: bool) -> anyhow::Result<()> {
    serve_with_shutdown(engine, bind, token, cors, std::future::pending()).await
}

/// Like [`serve`], but stops accepting connections once `shutdown` resolves
/// (the menu-bar app toggles the API from its status menu).
pub async fn serve_with_shutdown<F>(
    engine: Arc<Engine>,
    bind: &str,
    token: Option<String>,
    cors: bool,
    shutdown: F,
) -> anyhow::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("Bugle API listening on http://{bind}/v1  (token: {})", if token.is_some() { "required" } else { "none" });
    axum::serve(listener, router(engine, token, cors)).with_graceful_shutdown(shutdown).await?;
    Ok(())
}

async fn auth(State(state): State<AppState>, headers: HeaderMap, req: Request<Body>, next: Next) -> Response {
    if let Some(expected) = &state.token {
        let ok = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| t == expected)
            .unwrap_or(false)
            || headers.get("x-bugle-token").and_then(|v| v.to_str().ok()).map(|t| t == expected).unwrap_or(false);
        if !ok {
            return (StatusCode::UNAUTHORIZED, Json(json!({"ok": false, "error": "missing or invalid token"}))).into_response();
        }
    }
    next.run(req).await
}

fn respond(result: anyhow::Result<actions::ActionOutput>) -> Response {
    match result {
        Ok(out) => {
            let mut body = match out.json {
                Value::Object(m) => Value::Object(m),
                other => json!({"result": other}),
            };
            body["ok"] = json!(true);
            if let Some(img) = out.image {
                body["image_base64"] = json!(base64::engine::general_purpose::STANDARD.encode(img));
                body["image_mime"] = json!("image/png");
            }
            Json(body).into_response()
        }
        Err(e) => {
            let status = StatusCode::from_u16(status_for(&e)).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(json!({"ok": false, "error": format!("{e:#}")}))).into_response()
        }
    }
}

async fn action_post(State(state): State<AppState>, Path(name): Path<String>, Json(args): Json<Value>) -> Response {
    respond(actions::run(&state.engine, &name, args).await)
}

async fn action_get(State(state): State<AppState>, name: &'static str) -> Response {
    respond(actions::run(&state.engine, name, json!({})).await)
}

async fn windows(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let all = q.get("all").map(|v| v == "true" || v == "1").unwrap_or(false);
    respond(actions::run(&state.engine, "list_windows", json!({"all": all})).await)
}

async fn history(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let limit = q.get("limit").and_then(|v| v.parse::<u64>().ok()).unwrap_or(20);
    respond(actions::run(&state.engine, "history", json!({"limit": limit})).await)
}

async fn media_get(State(state): State<AppState>, Path(id): Path<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    let inline = q.get("inline").map(|v| v == "true" || v == "1").unwrap_or(false);
    respond(actions::run(&state.engine, "get_media", json!({"source": id, "inline_image": inline})).await)
}

async fn media_delete(State(state): State<AppState>, Path(id): Path<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    let f = q.get("file").map(|v| v == "true" || v == "1").unwrap_or(false);
    respond(actions::run(&state.engine, "delete_media", json!({"id": id, "delete_file": f})).await)
}

async fn media_file(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let media = match state.engine.resolve_media(&id) {
        Ok(m) => m,
        Err(e) => return respond(Err(e)),
    };
    match tokio::fs::read(&media.path).await {
        Ok(bytes) => {
            let mime = match media.path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
                Some("png") => "image/png",
                Some("jpg") | Some("jpeg") => "image/jpeg",
                Some("webp") => "image/webp",
                Some("gif") => "image/gif",
                Some("mov") => "video/quicktime",
                Some("mp4") => "video/mp4",
                Some("srt") | Some("vtt") | Some("txt") => "text/plain; charset=utf-8",
                _ => "application/octet-stream",
            };
            ([(header::CONTENT_TYPE, mime)], bytes).into_response()
        }
        Err(e) => (StatusCode::NOT_FOUND, Json(json!({"ok": false, "error": e.to_string()}))).into_response(),
    }
}

async fn health() -> Json<Value> {
    Json(json!({"ok": true, "name": "bugle", "version": env!("CARGO_PKG_VERSION")}))
}

async fn tools() -> Json<Value> {
    Json(json!(catalog().iter().map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.input_schema})).collect::<Vec<_>>()))
}

/// A minimal OpenAPI 3.1 document generated from the action catalog.
async fn openapi() -> Json<Value> {
    let mut paths = serde_json::Map::new();
    for t in catalog() {
        paths.insert(
            format!("/v1/actions/{}", t.name),
            json!({
                "post": {
                    "operationId": t.name,
                    "summary": t.description,
                    "requestBody": {"content": {"application/json": {"schema": t.input_schema}}},
                    "responses": {"200": {"description": "JSON result; `image_base64` holds a PNG when an image is returned"}}
                }
            }),
        );
    }
    Json(json!({
        "openapi": "3.1.0",
        "info": {"title": "Bugle", "version": env!("CARGO_PKG_VERSION"), "description": "Screenshots, recordings, OCR and AI actions for agents."},
        "paths": paths
    }))
}
