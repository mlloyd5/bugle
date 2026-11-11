//! JSON HTTP API (`bugle serve`).
//!
//! Every action is reachable as `POST /v1/actions/{name}`; the REST-style
//! aliases below exist for readability. Responses are
//! `{ "ok": true, ...result, "image_base64"?: "..." }` or
//! `{ "ok": false, "error": "..." }`.
//!
//! Security: when the listener is bound to loopback, requests whose `Host`
//! is not a loopback name are refused (DNS rebinding). A non-loopback bind
//! requires a token, and CORS is only enabled when a token is set.

use crate::actions::{self, catalog, status_for};
use axum::{
    body::Body,
    extract::{rejection::JsonRejection, Path, Query, State},
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
    /// Refuse requests whose `Host` is not a loopback name (set when the
    /// listener is bound to loopback).
    pub loopback_only: bool,
}

/// The API router for a loopback listener (non-loopback `Host` headers are
/// refused). See [`router_for_bind`].
pub fn router(engine: Arc<Engine>, token: Option<String>, cors: bool) -> Router {
    router_for_bind(engine, token, cors, true)
}

/// The API router. `loopback_only` turns on the `Host` check against DNS
/// rebinding. CORS is only enabled when a token is set.
pub fn router_for_bind(engine: Arc<Engine>, token: Option<String>, cors: bool, loopback_only: bool) -> Router {
    let token = token.filter(|t| !t.is_empty());
    let cors = if cors && token.is_none() {
        tracing::warn!("[api] cors = true needs a token; CORS stays off so web pages cannot drive the API");
        false
    } else {
        cors
    };
    let state = AppState { engine, token, loopback_only };
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
        .route("/v1/record/stop", post(|s| action_post(s, Path("record_stop".into()), Ok(Json(json!({}))))))
        .route("/v1/record/status", get(|s| action_get(s, "record_status")))
        .route("/v1/pin", post(|s, j| action_post(s, Path("pin".into()), j)))
        .route("/v1/copy", post(|s, j| action_post(s, Path("copy".into()), j)))
        .route("/v1/sync", post(|s, j| action_post(s, Path("sync".into()), j)))
        .route("/v1/select", post(|s| action_post(s, Path("select_area".into()), Ok(Json(json!({}))))))
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
    let token = token.filter(|t| !t.is_empty());
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host(bind)
        .await
        .map_err(|e| anyhow::anyhow!("invalid bind address {bind:?}: {e}"))?
        .collect();
    let loopback = !addrs.is_empty() && addrs.iter().all(|a| a.ip().is_loopback());
    if !loopback && token.is_none() {
        anyhow::bail!(
            "refusing to serve the API on non-loopback address {bind} without a token; \
             set [api] token (or --token / BUGLE_TOKEN), or bind to 127.0.0.1"
        );
    }
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("Bugle API listening on http://{bind}/v1  (token: {})", if token.is_some() { "required" } else { "none" });
    axum::serve(listener, router_for_bind(engine, token, cors, loopback)).with_graceful_shutdown(shutdown).await?;
    Ok(())
}

/// Constant-time byte comparison (the length is not secret).
fn token_eq(given: &str, expected: &str) -> bool {
    let (a, b) = (given.as_bytes(), expected.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    std::hint::black_box(a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y))) == 0
}

/// Whether a `Host` value (`name[:port]`) names the loopback interface.
fn is_loopback_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        // `[v6]` or `[v6]:port`
        match rest.split_once(']') {
            Some((ip, tail)) if tail.is_empty() || tail.starts_with(':') => ip,
            _ => return false,
        }
    } else {
        host.rsplit_once(':').map_or(host, |(name, _port)| name)
    };
    name.eq_ignore_ascii_case("localhost") || name.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({"ok": false, "error": msg.into()}))).into_response()
}

async fn auth(State(state): State<AppState>, headers: HeaderMap, req: Request<Body>, next: Next) -> Response {
    if state.loopback_only {
        // HTTP/2 carries the host in the URI authority instead of `Host`.
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .or_else(|| req.uri().authority().map(|a| a.as_str().to_string()));
        if !host.as_deref().is_some_and(is_loopback_host) {
            return error(StatusCode::FORBIDDEN, "Host header must be a loopback address (127.0.0.1, localhost or [::1])");
        }
    }
    if let Some(expected) = &state.token {
        let ok = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| token_eq(t, expected))
            .unwrap_or(false)
            || headers.get("x-bugle-token").and_then(|v| v.to_str().ok()).map(|t| token_eq(t, expected)).unwrap_or(false);
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

async fn action_post(
    State(state): State<AppState>,
    Path(name): Path<String>,
    args: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Json(args) = match args {
        Ok(a) => a,
        Err(rej) => return error(rej.status(), rej.body_text()),
    };
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
    // History items only (id, id prefix, `latest`): never a raw file path.
    let found = if id == "latest" || id == "last" { state.engine.history.latest() } else { state.engine.history.get(&id) };
    let media = match found {
        Ok(Some(m)) => m,
        Ok(None) => return error(StatusCode::NOT_FOUND, format!("media not found in history: {id}")),
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
        Err(e) => error(StatusCode::NOT_FOUND, e.to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use bugle_core::{config::ApiConfig, Config, MediaItem, MediaKind};
    use tower::ServiceExt;

    struct Fixture {
        tmp: tempfile::TempDir,
        engine: Arc<Engine>,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.general.save_dir = tmp.path().join("save").display().to_string();
        cfg.general.history_path = Some(tmp.path().join("history.jsonl").display().to_string());
        let engine = Arc::new(Engine::new(cfg, false).unwrap());
        Fixture { tmp, engine }
    }

    fn get(uri: &str, host: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().uri(uri);
        if let Some(h) = host {
            b = b.header(header::HOST, h);
        }
        b.body(Body::empty()).unwrap()
    }

    async fn send(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let res = app.clone().oneshot(req).await.unwrap();
        let (parts, body) = res.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        (parts.status, parts.headers, bytes.to_vec())
    }

    fn envelope(body: &[u8]) {
        let v: Value = serde_json::from_slice(body).expect("JSON body");
        assert_eq!(v["ok"], json!(false), "{v}");
        assert!(v["error"].as_str().is_some_and(|e| !e.is_empty()), "{v}");
    }

    #[test]
    fn token_comparison() {
        assert!(token_eq("secret", "secret"));
        assert!(!token_eq("secreT", "secret"));
        assert!(!token_eq("secret1", "secret"));
        assert!(!token_eq("", "secret"));
    }

    #[test]
    fn loopback_host_names() {
        for h in ["127.0.0.1", "127.0.0.1:4848", "localhost", "LOCALHOST:4848", "[::1]", "[::1]:4848"] {
            assert!(is_loopback_host(h), "{h}");
        }
        for h in ["evil.example", "evil.example:4848", "localhost.evil.example", "192.168.1.2:4848", "[::2]:4848", "[::1]x", "::1", ""] {
            assert!(!is_loopback_host(h), "{h}");
        }
    }

    #[tokio::test]
    async fn auth_requires_token_when_set() {
        let f = fixture();
        let app = router(f.engine.clone(), Some("secret".into()), false);
        let (status, _, body) = send(&app, get("/v1/health", Some("127.0.0.1:4848"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        envelope(&body);

        let mut req = get("/v1/health", Some("127.0.0.1:4848"));
        req.headers_mut().insert(header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
        assert_eq!(send(&app, req).await.0, StatusCode::UNAUTHORIZED);

        let mut req = get("/v1/health", Some("127.0.0.1:4848"));
        req.headers_mut().insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert_eq!(send(&app, req).await.0, StatusCode::OK);

        let mut req = get("/v1/health", Some("localhost:4848"));
        req.headers_mut().insert("x-bugle-token", "secret".parse().unwrap());
        assert_eq!(send(&app, req).await.0, StatusCode::OK);

        // An empty token counts as no token.
        let open = router(f.engine.clone(), Some(String::new()), false);
        assert_eq!(send(&open, get("/v1/health", Some("127.0.0.1"))).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn non_loopback_host_is_rejected() {
        let f = fixture();
        let app = router(f.engine.clone(), None, false);
        for h in ["127.0.0.1:4848", "localhost:4848", "[::1]:4848", "127.0.0.1"] {
            assert_eq!(send(&app, get("/v1/health", Some(h))).await.0, StatusCode::OK, "{h}");
        }
        for h in [Some("evil.example:4848"), Some("rebind.evil.example"), None] {
            let (status, _, body) = send(&app, get("/v1/health", h)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{h:?}");
            envelope(&body);
        }
        // The Host check runs before the token check; a valid token does not help.
        let app = router(f.engine.clone(), Some("secret".into()), false);
        let mut req = get("/v1/health", Some("evil.example"));
        req.headers_mut().insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert_eq!(send(&app, req).await.0, StatusCode::FORBIDDEN);
        // A non-loopback listener does not check the Host.
        let app = router_for_bind(f.engine.clone(), Some("secret".into()), false, false);
        let mut req = get("/v1/health", Some("bugle.lan:4848"));
        req.headers_mut().insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert_eq!(send(&app, req).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn media_file_serves_history_items_only() {
        let f = fixture();
        let app = router(f.engine.clone(), None, false);
        let shot = f.engine.config.save_dir().join("shot.png");
        std::fs::write(&shot, b"not really a png").unwrap();
        let item = MediaItem::new(MediaKind::Screenshot, &shot);
        f.engine.history.push(&item).unwrap();

        for id in [item.id.clone(), item.id[..6].to_string(), "latest".into()] {
            let (status, headers, body) = send(&app, get(&format!("/v1/media/{id}/file"), Some("127.0.0.1"))).await;
            assert_eq!(status, StatusCode::OK, "{id}");
            assert_eq!(headers[header::CONTENT_TYPE], "image/png");
            assert_eq!(body, b"not really a png");
        }

        // Raw paths are never served, not even the path of a history item.
        let outside = f.tmp.path().join("secret.txt");
        std::fs::write(&outside, b"secret").unwrap();
        for p in [outside.display().to_string(), shot.display().to_string(), "/etc/passwd".into()] {
            let enc = p.replace('/', "%2F");
            let (status, _, body) = send(&app, get(&format!("/v1/media/{enc}/file"), Some("127.0.0.1"))).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{p}");
            envelope(&body);
        }
        let (status, _, _) = send(&app, get("/v1/media/..%2F..%2Fetc%2Fpasswd/file", Some("127.0.0.1"))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    fn preflight() -> Request<Body> {
        Request::builder()
            .method("OPTIONS")
            .uri("/v1/capture")
            .header(header::HOST, "127.0.0.1:4848")
            .header(header::ORIGIN, "https://evil.example")
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn cors_needs_a_token() {
        assert!(!ApiConfig::default().cors);
        let f = fixture();
        for cors in [false, true] {
            let app = router(f.engine.clone(), None, cors);
            let (_, headers, _) = send(&app, preflight()).await;
            assert!(!headers.contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN), "cors={cors}");
            let mut req = get("/v1/health", Some("127.0.0.1:4848"));
            req.headers_mut().insert(header::ORIGIN, "https://evil.example".parse().unwrap());
            let (status, headers, _) = send(&app, req).await;
            assert_eq!(status, StatusCode::OK);
            assert!(!headers.contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN), "cors={cors}");
        }
        let app = router(f.engine.clone(), Some("secret".into()), true);
        let (_, headers, _) = send(&app, preflight()).await;
        assert!(headers.contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
    }

    #[tokio::test]
    async fn malformed_json_gets_the_envelope() {
        let f = fixture();
        let app = router(f.engine.clone(), None, false);
        for uri in ["/v1/actions/history", "/v1/capture"] {
            let req = Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::HOST, "127.0.0.1")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{not json"))
                .unwrap();
            let (status, _, body) = send(&app, req).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
            envelope(&body);
            let req = Request::builder().method("POST").uri(uri).header(header::HOST, "127.0.0.1").body(Body::from("{}")).unwrap();
            let (status, _, body) = send(&app, req).await;
            assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{uri}");
            envelope(&body);
        }
    }

    #[tokio::test]
    async fn non_loopback_bind_needs_a_token() {
        let f = fixture();
        for token in [None, Some(String::new())] {
            let err = serve_with_shutdown(f.engine.clone(), "0.0.0.0:0", token, false, async {}).await.unwrap_err();
            assert!(err.to_string().contains("without a token"), "{err}");
        }
        serve_with_shutdown(f.engine.clone(), "0.0.0.0:0", Some("secret".into()), false, async {}).await.unwrap();
        serve_with_shutdown(f.engine.clone(), "127.0.0.1:0", None, false, async {}).await.unwrap();
    }
}
