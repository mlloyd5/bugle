# 009 — HTTP API: web pages can fire body-less POSTs without a token

Status: closed (fixed on `fix/api-cross-origin-requests`)
Severity: medium
Area: `crates/bugle-api/src/http.rs`, docs

## Problem

With the defaults (loopback bind, no token, CORS off) the API relies on the browser's CORS preflight to keep web pages
out: every action takes `Content-Type: application/json`, which a page cannot send cross-origin without a preflight,
and with CORS off the preflight fails. Two routes take no body, so there is nothing to preflight:

- `POST /v1/select` raises the full-screen area-selection overlay.
- `POST /v1/record/stop` stops the recording in progress.

A page on any origin can send them with `fetch(url, {method: "POST", mode: "no-cors"})` or an auto-submitting form.
The browser sets `Host` to the loopback address, so the Host check from issue 001 passes. The page cannot read the
response, but the action has already run.

Reproduced on `main` at 8854c4a:

- Router test: `POST /v1/select` with `Origin: https://example.com` and no body returns `200 {"ok":true,"rect":null}`.
- `bugle serve --bind 127.0.0.1:48977` with no token, then
  `curl -X POST -H 'Origin: https://example.com' http://127.0.0.1:48977/v1/record/stop` returns
  `500 {"ok":false,"error":"no recording in progress"}`: the action ran. Adding `Sec-Fetch-Site: cross-site` and
  `Content-Type: text/plain` changes nothing.

Every other route was checked for the same property:

- POST routes with a JSON body, including `POST /v1/actions/{name}` (so `/v1/actions/select_area` and
  `/v1/actions/record_stop` too), answer 415 to `text/plain`, form encoding and a missing content type. Not reachable
  without a preflight.
- `DELETE /v1/media/{id}` always needs a preflight.
- GET routes need no preflight but change nothing, and the page cannot read the answer. They can still be embedded
  (`<img src=".../v1/media/latest/file">` reveals whether a capture exists and its size), so they are refused as well.

## Fix

In the `auth` middleware, so it covers every route and method: when no token is configured, refuse with 403 and the
JSON envelope any request that a browser marks as coming from another origin:

- an `Origin` header whose value is not `http://<Host>` of the request itself (`Origin: null` included), or
- a `Sec-Fetch-Site` header other than `same-origin` or `none`.

With a token configured nothing changes: such a page cannot attach the token without a preflight, and with
`cors = true` the token is what authorises it.

## Client-visible behaviour

- `curl`, HTTP libraries, the CLI and the MCP server (stdio) send neither header and work as before, with no new
  header and no content type needed on the body-less routes.
- With no token, a request carrying a foreign `Origin` now gets 403 on every route, GETs included
  (`GET /v1/health` with `Origin: https://example.com` used to answer 200). A client that sets `Origin` by hand must
  drop it or use a token.
- Browser pages on another origin need a token and `cors = true`, as documented before. A browser extension that
  holds host permissions for the API's address is not stopped: it can send requests without `Origin`, like any other
  local process.

## Tests

Router tests in `http.rs`: cross-origin request refused on every route, and in several header shapes on the two
body-less POSTs; the same requests without `Origin` (or with the API's own origin) still reach the action; behaviour
with a token unchanged with CORS on and off. The accepted case is asserted on `/v1/record/stop` only, because an
accepted `/v1/select` starts the selection helper.

## Notes

Current browsers send `Sec-Fetch-Site` on every request and `Origin` on every cross-origin POST. A browser too old
for `Sec-Fetch-Site` is covered on the state-changing routes only as far as it sends `Origin`: some old releases
(Firefox before about version 70, as far as we know) left it off cross-origin form POSTs, so an auto-submitting form
there could still reach the two body-less routes. Embedded GETs pass on such browsers too. A token closes both.
