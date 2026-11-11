# 001 — HTTP API: any web page can drive the API with default settings

Status: closed (fixed on `audit/fixes`)
Severity: high
Area: `crates/bugle-api/src/http.rs`, `ApiConfig` in `crates/bugle-core/src/config.rs`, docs

## Problems

1. **CORS is on by default and there is no token by default.** `ApiConfig::default()` has `cors: true`, `token: None`, and
   `router()` applies `CorsLayer::very_permissive()`. With `bugle serve` (or the menu-bar app's "Start Agent API") running,
   any page open in the user's browser can `fetch("http://127.0.0.1:4848/v1/capture")` and read the screenshot back, read
   history, start recordings, and so on.
2. **Arbitrary file read.** `GET /v1/media/{id}/file` (`media_file`, http.rs:155) resolves `{id}` with
   `Engine::resolve_media`, which accepts any file path. `GET /v1/media/%2Fetc%2Fpasswd/file` returns the file.
3. **No Host check**, so a loopback bind is reachable through DNS rebinding even with CORS off.
4. **Non-loopback bind with no token is accepted silently** (`--bind 0.0.0.0:4848`).
5. Token comparison is not constant-time (http.rs:96-98).
6. A malformed JSON body gets axum's plain-text rejection instead of the documented `{"ok":false,"error":...}` envelope.

## Wanted

- `cors` defaults to `false`. When `cors` is true and no token is configured, do not enable CORS: log a warning that
  CORS needs a token and carry on without it.
- `serve_with_shutdown` refuses to start (clear error) when the bind address is not loopback and no token is set.
  Do this inside `bugle-api` so neither the CLI nor the app has to change.
- Reject requests whose `Host` header is not a loopback name/IP (`127.0.0.1`, `localhost`, `[::1]`, with or without
  port) when the listener is bound to loopback. 403 with the JSON envelope.
- `/v1/media/{id}/file` serves only items found in history (id, id prefix, `latest`), never raw paths.
- Constant-time token comparison (no new dependency needed; a small fold over bytes is fine).
- JSON envelope for body rejections on the action routes.
- Update `README.md`, `docs/AGENTS.md`, `config.example.toml` to match (token recommended; CORS needs a token).
- Tests: router-level tests (tower `oneshot`) for auth, Host rejection, path-source rejection on `/file`, CORS-off-without-token.
  Use a temp `history_path`/`save_dir` like the sync tests do; the tests must not need Screen Recording permission.

## Out of scope

`crates/bugle-cli`, `crates/bugle-app`, `actions.rs`, `mcp.rs` (other issues own them).
