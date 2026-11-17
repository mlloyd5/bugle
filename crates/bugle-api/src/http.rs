//! JSON HTTP API (`bugle serve`).
//!
//! Every action is reachable as `POST /v1/actions/{name}`; the REST-style
//! aliases below exist for readability. Responses are
//! `{ "ok": true, ...result, "image_base64"?: "..." }` or
//! `{ "ok": false, "error": "..." }`.
//!
//! Security: when the listener is bound to loopback, requests whose `Host`
//! is not a loopback name are refused (DNS rebinding). A non-loopback bind
//! requires a token, and CORS is only enabled when a token is set. Without a
//! token, requests a browser marks as coming from another origin are refused
//! on every route, since some need no CORS preflight.

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
        .route("/v1/edit", post(|s, j| action_post(s, Path("edit_video".into()), j)))
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
        // Every route must be added above this line: a route added after the
        // layer skips the Host, cross-origin and token checks. New routes also
        // go in `ROUTES` in the tests below.
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

/// Whether the request was sent by a web page on another origin. Browsers
/// attach `Origin` to every cross-origin request that can change state and
/// `Sec-Fetch-Site` to all requests; other clients send neither. `host` is the
/// request's own `Host`, so a page served from this listener still passes.
/// The listener only speaks plain HTTP, so an `https://` origin is never its
/// own (with the port left out, `https://127.0.0.1` would otherwise match a
/// listener on port 80).
fn is_cross_origin(headers: &HeaderMap, host: Option<&str>) -> bool {
    if headers.get("sec-fetch-site").is_some_and(|v| !matches!(v.as_bytes(), b"same-origin" | b"none")) {
        return true;
    }
    let Some(origin) = headers.get(header::ORIGIN) else { return false };
    let authority = origin.to_str().ok().and_then(|o| o.strip_prefix("http://"));
    !matches!((authority, host), (Some(a), Some(h)) if a.eq_ignore_ascii_case(h))
}

async fn auth(State(state): State<AppState>, headers: HeaderMap, req: Request<Body>, next: Next) -> Response {
    // HTTP/2 carries the host in the URI authority instead of `Host`.
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| req.uri().authority().map(|a| a.as_str().to_string()));
    if state.loopback_only && !host.as_deref().is_some_and(is_loopback_host) {
        return error(StatusCode::FORBIDDEN, "Host header must be a loopback address (127.0.0.1, localhost or [::1])");
    }
    // Without a token nothing else stops a web page: a body-less POST (and any
    // GET) needs no CORS preflight, so the browser would simply send it.
    if state.token.is_none() && is_cross_origin(&headers, host.as_deref()) {
        return error(StatusCode::FORBIDDEN, "cross-origin requests need a token: set [api] token (and cors = true for web pages)");
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
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert!(!headers.contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN), "cors={cors}");
        }
        let app = router(f.engine.clone(), Some("secret".into()), true);
        let (_, headers, _) = send(&app, preflight()).await;
        assert!(headers.contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
    }

    /// A request with no body and only headers a browser adds by itself: what
    /// a page on another origin can send without a preflight.
    fn bare(method: &str, uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri).header(header::HOST, "127.0.0.1:4848");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Body::empty()).unwrap()
    }

    /// Every route; the body-less POSTs come first.
    /// Refused by the cross-origin check itself, not by the Host check or an
    /// action that answers 403 for its own reasons.
    fn refused_as_cross_origin(status: StatusCode, body: &[u8], what: &str) {
        let text = String::from_utf8_lossy(body);
        assert_eq!(status, StatusCode::FORBIDDEN, "{what}: {text}");
        assert!(text.contains("cross-origin"), "{what}: {text}");
        envelope(body);
    }

    const ROUTES: &[(&str, &str)] = &[
        ("POST", "/v1/select"),
        ("POST", "/v1/record/stop"),
        ("POST", "/v1/actions/select_area"),
        ("POST", "/v1/actions/record_stop"),
        ("POST", "/v1/capture"),
        ("POST", "/v1/scroll"),
        ("POST", "/v1/ocr"),
        ("POST", "/v1/describe"),
        ("POST", "/v1/annotate"),
        ("POST", "/v1/transcribe"),
        ("POST", "/v1/subtitles"),
        ("POST", "/v1/complete"),
        ("POST", "/v1/edit"),
        ("POST", "/v1/record/start"),
        ("POST", "/v1/pin"),
        ("POST", "/v1/copy"),
        ("POST", "/v1/sync"),
        ("DELETE", "/v1/media/latest"),
        ("GET", "/v1/media/latest"),
        ("GET", "/v1/media/latest/file"),
        ("GET", "/v1/health"),
        ("GET", "/v1/openapi.json"),
        ("GET", "/v1/tools"),
        ("GET", "/v1/annotate/schema"),
        ("GET", "/v1/record/status"),
        ("GET", "/v1/displays"),
        ("GET", "/v1/windows"),
        ("GET", "/v1/providers"),
        ("GET", "/v1/permissions"),
        ("GET", "/v1/history"),
    ];

    #[tokio::test]
    async fn cross_origin_requests_are_refused_without_a_token() {
        let f = fixture();
        let app = router(f.engine.clone(), None, false);
        for (method, uri) in ROUTES {
            let (status, _, body) = send(&app, bare(method, uri, &[("origin", "https://example.com")])).await;
            refused_as_cross_origin(status, &body, &format!("{method} {uri}"));
        }
        // Other shapes of the same request, on the two body-less POST routes.
        let foreign: &[&[(&str, &str)]] = &[
            &[("origin", "null")],
            &[("origin", "http://localhost:3000")],
            &[("origin", "http://127.0.0.1:4848.example.com")],
            &[("origin", "https://127.0.0.1:4848")],
            &[("origin", "https://example.com"), ("sec-fetch-site", "cross-site")],
            &[("sec-fetch-site", "cross-site")],
            &[("sec-fetch-site", "same-site")],
            &[("origin", "https://example.com"), ("content-type", "text/plain")],
            &[("origin", "https://example.com"), ("content-type", "application/x-www-form-urlencoded")],
        ];
        for uri in ["/v1/select", "/v1/record/stop"] {
            for headers in foreign {
                let (status, _, body) = send(&app, bare("POST", uri, headers)).await;
                refused_as_cross_origin(status, &body, &format!("{uri} {headers:?}"));
            }
        }
        // `cors = true` without a token changes nothing.
        let app = router(f.engine.clone(), None, true);
        let (status, _, body) = send(&app, bare("POST", "/v1/record/stop", &[("origin", "https://example.com")])).await;
        refused_as_cross_origin(status, &body, "cors without a token");
    }

    /// The action ran: with no recording in progress `record_stop` fails with
    /// its own error rather than a refusal from the middleware.
    fn reached_record_stop(status: StatusCode, body: &[u8]) {
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{}", String::from_utf8_lossy(body));
        assert!(String::from_utf8_lossy(body).contains("no recording in progress"));
    }

    #[tokio::test]
    async fn requests_from_non_browser_clients_need_no_extra_headers() {
        let f = fixture();
        let app = router(f.engine.clone(), None, false);
        // (`/v1/select` shares this path but would start the selection helper.)
        let local: &[&[(&str, &str)]] = &[
            &[],
            &[("origin", "http://127.0.0.1:4848")],
            &[("origin", "http://127.0.0.1:4848"), ("sec-fetch-site", "same-origin")],
            &[("sec-fetch-site", "none")],
        ];
        for uri in ["/v1/record/stop", "/v1/actions/record_stop"] {
            for headers in local {
                // The catch-all route takes a JSON body like every other action.
                let mut req = bare("POST", uri, headers);
                if uri.starts_with("/v1/actions/") {
                    req.headers_mut().insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
                    *req.body_mut() = Body::from("{}");
                }
                let (status, _, body) = send(&app, req).await;
                reached_record_stop(status, &body);
            }
        }
        let req = Request::builder()
            .method("POST")
            .uri("/v1/actions/history")
            .header(header::HOST, "127.0.0.1:4848")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(send(&app, req).await.0, StatusCode::OK);
        for uri in ["/v1/health", "/v1/history", "/v1/record/status", "/v1/tools"] {
            assert_eq!(send(&app, bare("GET", uri, &[])).await.0, StatusCode::OK, "{uri}");
        }
    }

    #[tokio::test]
    async fn a_token_decides_alone_when_one_is_set() {
        let f = fixture();
        let origin = ("origin", "https://example.com");
        let token = ("authorization", "Bearer secret");
        for cors in [false, true] {
            let app = router(f.engine.clone(), Some("secret".into()), cors);
            for uri in ["/v1/select", "/v1/record/stop"] {
                let (status, _, body) = send(&app, bare("POST", uri, &[origin])).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} cors={cors}");
                envelope(&body);
            }
            for sent in [&[token][..], &[origin, token], &[origin, token, ("sec-fetch-site", "cross-site")]] {
                let (status, headers, body) = send(&app, bare("POST", "/v1/record/stop", sent)).await;
                reached_record_stop(status, &body);
                assert_eq!(headers.contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN), cors && sent.contains(&origin), "{sent:?}");
            }
        }
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
    async fn edit_alias_trims_a_recording() {
        let f = fixture();
        let app = router(f.engine.clone(), None, false);
        let src = f.tmp.path().join("in.gif");
        crate::actions::tests::fixture_gif(&src);
        let post = |body: Value| {
            Request::builder()
                .method("POST")
                .uri("/v1/edit")
                .header(header::HOST, "127.0.0.1")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let (status, _, body) = send(&app, post(json!({"source": src, "end": 0.3, "preset": "gif-small"}))).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["media"]["kind"], "gif");
        assert_eq!(v["media"]["duration_seconds"], 0.3);
        assert!(std::path::Path::new(v["media"]["path"].as_str().unwrap()).exists());
        let (status, _, body) = send(&app, post(json!({"source": src, "preset": "4k"}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        envelope(&body);
    }

    #[tokio::test]
    async fn pick_window_keeps_the_security_checks() {
        let f = fixture();
        let post = |host: &str, token: Option<&str>| {
            let mut b = Request::builder()
                .method("POST")
                .uri("/v1/capture")
                .header(header::HOST, host)
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(t) = token {
                b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
            }
            b.body(Body::from(json!({"target": {"kind": "pick_window"}}).to_string())).unwrap()
        };
        let app = router(f.engine.clone(), Some("secret".into()), false);
        // The overlay is never requested before Host and token pass.
        assert_eq!(send(&app, post("evil.example", Some("secret"))).await.0, StatusCode::FORBIDDEN);
        assert_eq!(send(&app, post("127.0.0.1", None)).await.0, StatusCode::UNAUTHORIZED);
        // Authorised, but `bugle serve` has no UI host: a clear 503.
        let (status, _, body) = send(&app, post("127.0.0.1", Some("secret"))).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        envelope(&body);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert!(v["error"].as_str().unwrap().contains("menu-bar app"), "{v}");
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
