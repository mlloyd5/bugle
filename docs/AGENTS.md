# Bugle for agents

Bugle exposes one action catalog over two transports. Use whichever your agent
runtime speaks.

## MCP (stdio)

```bash
# Claude Code
claude mcp add bugle -- /path/to/bugle mcp
# or with a dedicated config
claude mcp add bugle -e BUGLE_CONFIG=/path/to/config.toml -- /path/to/bugle mcp
```

Claude Desktop / any MCP client (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "bugle": { "command": "/path/to/bugle", "args": ["mcp"] }
  }
}
```

`tools/call` results carry a `text` block with the JSON result and, for
`capture`, `annotate`, `get_media` (and `scroll_capture` when `inline_image` is
true), an `image` block with a PNG — so the model can look at the screenshot it
just took. Pass `max_edge` (default 1600 px) to control how large that inline image
is.

A typical loop:

1. `list_windows` → pick the window id of the app under test.
2. `capture` `{ "target": {"kind":"window","id":1234} }` → look at the image, note
   the media id.
3. `ocr` `{ "source": "<id>" }` → text with pixel bounding boxes; use the boxes to
   compute coordinates for the next step.
4. `annotate` `{ "source": "<id>", "annotation": {...} }` → a marked-up copy to
   hand back to a human.
5. `record_start` → drive the app → `record_stop` → `subtitles`.

## HTTP

```bash
bugle serve --bind 127.0.0.1:4848 --token secret     # alias: bugle jameson
```

* All actions: `POST /v1/actions/{name}` with the tool's JSON arguments.
* Aliases: `POST /v1/capture`, `/v1/ocr`, `/v1/describe`, `/v1/annotate`,
  `/v1/scroll`, `/v1/transcribe`, `/v1/subtitles`, `/v1/edit`, `/v1/complete`,
  `/v1/record/start|stop`, `/v1/pin`, `/v1/copy`, `/v1/sync`, `/v1/select`;
  `GET /v1/displays`, `/v1/windows?all=1`, `/v1/history?limit=20`,
  `/v1/record/status`, `/v1/providers`, `/v1/permissions`, `/v1/tools`,
  `/v1/openapi.json`, `/v1/annotate/schema`, `/v1/media/{id}`,
  `/v1/media/{id}/file`; `DELETE /v1/media/{id}?file=1`.
* Responses: `{"ok":true, ...}` with `image_base64`/`image_mime` when an image is
  returned; errors (including malformed JSON bodies) are `{"ok":false,"error":"..."}`
  with a 4xx/5xx status.
* Auth: `Authorization: Bearer <token>` or `X-Bugle-Token: <token>` when a token is
  configured (`[api] token` or `--token`/`BUGLE_TOKEN`). A token is recommended, and
  required when binding to a non-loopback address (the server refuses to start
  otherwise).
* On a loopback bind, requests whose `Host` is not `127.0.0.1`, `localhost` or
  `[::1]` (with or without port) get 403, which blocks DNS-rebinding pages.
* CORS (`[api] cors = true`) is off by default and only enabled when a token is
  set; without one it is ignored with a warning.
* `/v1/media/{id}/file` serves history items only (id, id prefix or `latest`),
  never arbitrary file paths.

```bash
H='Authorization: Bearer secret'; U=http://127.0.0.1:4848/v1
curl -s -H "$H" -H 'content-type: application/json' \
  -d '{"target":{"kind":"area","rect":{"x":0,"y":30,"w":1280,"h":720}},"max_edge":800}' $U/capture \
  | python3 -c 'import sys,json,base64; d=json.load(sys.stdin); open("shot.png","wb").write(base64.b64decode(d["image_base64"])); print(d["media"]["id"])'
curl -s -H "$H" -H 'content-type: application/json' -d '{"source":"latest"}' $U/ocr
curl -s -H "$H" -H 'content-type: application/json' \
  -d '{"source":"latest","annotation":{"shapes":[{"type":"arrow","from":{"x":10,"y":10},"to":{"x":300,"y":200}}],"canvas":{"padding":32,"background":"mono"}}}' $U/annotate
```

## Tool reference

| Tool | Arguments | Returns |
|---|---|---|
| `capture` | `target`, `delay`, `format`, `quality`, `copy`, `output`, `label`, `shadow`, `hide_cursor`, `inline_image`, `max_edge` | `media`, `rect`, `window` + image |
| `list_displays` | – | displays |
| `list_windows` | `all` | windows, front-most first |
| `ocr` | `source`, `languages`, `fast`, `region`, `engine` | `text`, `lines[{text,bbox,confidence}]` |
| `describe` | `source`, `prompt`, `provider`, `with_ocr` | `description`, `provider`, `model`, `ocr?` |
| `annotate` | `source`, `annotation`, `output`, `copy`, `inline_image` | `media` + image |
| `annotate_schema` | – | example document |
| `scroll_capture` | `rect`, `max_frames`, `step_lines`, `settle_ms`, `horizontal`, `output`, `inline_image` | `media`, `stats` |
| `record_start` | `target`, `format` (mp4/mov/gif), `fps`, `microphone`, `max_seconds`, `highlight_clicks`, `output`, `label` | status |
| `record_stop` | – | `media` |
| `record_status` | – | status |
| `transcribe` | `source`, `provider`, `language` | `transcript`, `srt` |
| `subtitles` | `source`, `format` (srt/vtt), `provider`, `language`, `output` | `path`, `segments`, `text` |
| `edit_video` | `source`, `start`, `end` (seconds, negative = from the end), `preset` (original/high/hevc/1080p/720p/540p/gif/gif-small), `fps`, `max_size`, `output`, `label` | `media` (parent = source) |
| `complete` | `prompt`, `provider`, `system` | `text` |
| `history` | `limit` | media list |
| `get_media` | `source`, `inline_image`, `max_edge` | media (+ image) |
| `delete_media` | `id`, `delete_file` | `deleted` |
| `pin` | `source`, `frame`, `opacity` | `pinned` |
| `copy` | `source` | `copied` |
| `sync` | `source` | `media` (new `path`/`url`), `sync` (`action`, `destination`) |
| `select_area` | – | `rect` (user drags) |
| `providers` | – | provider list with roles |
| `permissions` | – | `screen_recording`, `accessibility` |

`source` accepts a media id (or prefix), `latest`, or a file path.
Target coordinates are screen points (top-left origin); annotation and OCR
coordinates are image pixels.
