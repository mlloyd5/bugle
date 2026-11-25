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
* With no token, a request that a browser marks as coming from another origin
  gets 403 on every route and method: an `Origin` header that is not the API's
  own address (`http://<Host>`), or `Sec-Fetch-Site` other than `same-origin` or
  `none`. This covers the requests browsers send without a CORS preflight
  (body-less `POST /v1/select` and `/v1/record/stop`, form posts, plain GETs).
  Clients that send neither header (`curl`, HTTP libraries, agents) are
  unaffected; do not add an `Origin` header by hand. With a token configured the
  token alone decides, and web pages need the token plus `cors = true`.
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
| `record_start` | `target`, `format` (mp4/mov/gif), `fps`, `microphone`, `system_audio`, `mix_audio`, `microphone_gain`, `system_audio_gain`, `ducking`, `ducking_amount_db` (levels of the mixed track, or of each separate track without `mix_audio`; defaults from `[recordings]`), `max_seconds`, `highlight_clicks`, `cursor_events` (writes `<file>.events.json`), `hide_cursor` (pointer left out of the frames; implies `cursor_events`), `output`, `label` | status (HTTP 409 `busy` while the menu-bar app is still saving a recording that stopped at its `max_seconds`; retry a moment later) |
| `record_stop` | – | `media` (after a `max_seconds` auto-stop the menu-bar app already finalised: that recording's, or its finalisation error with the same status as a failed stop, once, within 10 minutes and before the next start) |
| `record_status` | – | status (`audio_levels` while capturing audio, see below) |
| `transcribe` | `source`, `provider`, `language` | `transcript`, `srt` |
| `subtitles` | `source`, `format` (srt/vtt), `provider`, `language`, `output` | `path`, `segments`, `text` |
| `edit_video` | `source`, `start`, `end` (seconds, negative = from the end), `cuts` (`[{start, end}]` ranges to remove; the rest is joined), `preset` (original/lossless/high/hevc/1080p/720p/540p/gif/gif-small), `fps` (re-encodes video; resamples GIF), `max_size` (GIF long edge), `zoom` (factor above 1, at most 4: zoom in on clicks), `click_zooms` (`[{"t": 4.2, "zoom": 3}]`: one click's level, by its time; overrides `zoom`, 1 leaves the click out), `smooth_cursor` (draw a smoothed pointer; `hide_cursor` recordings), `output`, `label` | `media` (parent = source when it is a history item) |
| `complete` | `prompt`, `provider`, `system` | `text` |
| `history` | `limit` | media list, newest first (entries whose file was deleted are hidden, not removed) |
| `history_prune` | `dry_run` | `pruned` (entries removed, or that would be; files are never touched) |
| `get_media` | `source`, `inline_image`, `max_edge` | media (+ image) |
| `delete_media` | `id`, `delete_file` | `deleted` |
| `pin` | `source`, `frame`, `opacity` | `pinned` |
| `copy` | `source` | `copied` |
| `sync` | `source` | `media` (new `path`/`url`), `sync` (`action`, `destination`, `sidecars`) |
| `select_area` | – | `rect` (user drags) |
| `providers` | – | provider list with roles |
| `permissions` | – | `screen_recording`, `accessibility`, `microphone` (`authorized`, `denied`, `restricted` or `not_determined`; read without prompting) |

`edit_video` with `original` copies video samples only when the cut starts on a
keyframe and re-encodes otherwise, so trimmed-off footage is not left in the
file. `lossless` always copies: faster and without quality loss, but a cut
between keyframes keeps up to one keyframe interval (2 s for Bugle's own
recordings) before the start in the file, hidden by an edit list and
recoverable. One engine runs at most two edits at a time; a third is refused.

`cuts` removes several ranges in one export: each `{start, end}` is in
seconds (negative = from the end), cuts may overlap or reach outside
`start..end`, and the parts left (kept parts under 10 ms are dropped) are
joined back to back, audio included. `original` copies only when every kept
part starts on a keyframe. Cuts that leave less than 0.05 s are a 400. A GIF
frame a cut splits keeps the part shown on each side. A plain edit with cuts
writes the event track spliced the same way.

`zoom` and `smooth_cursor` read the recording's event track (made by
`record_start` with `cursor_events` or `hide_cursor`); without one they are a
400. They re-render every frame (at the recording's frame rate, at least 30
fps), so `lossless` is refused with them and `original`/`high` encode H.264
(`hevc`: HEVC); the audio tracks are copied back in untouched. Zoom follows
clicks inside the recorded area only. `click_zooms` entries must each name one
of those clicks (the nearest within 0.05 s), else the edit is a 400 that lists
them; without `zoom` only the named clicks zoom. `click_zooms` times are on the
source recording's timeline, even when `cuts` are present. The track is `<file>.events.json` (the
whole file name, so `clip.mp4` and `clip.mov` each have their own), and an
effects edit's output has none.

While a recording captures audio with the ScreenCaptureKit recorder (system
audio and/or the microphone), `record_status` (and the status
`record_start` returns) carries `audio_levels`:
`{"microphone": {"peak_db": -18.2, "rms_db": -31.0}, "system_audio": null}`.
Values are dBFS (0 = full scale), floored at -100 for silence, measured as
recorded (before `microphone_gain`, `system_audio_gain` and ducking). The peak
falls at 20 dB/s once the signal drops and the RMS is averaged over 0.3 s, so
polling a few times a second is enough. A source the recording does not
capture (a microphone that could not be opened, or one whose sample format
the meter cannot read) is `null`; the field is
absent when idle, finishing, recording a GIF, or on the `screencapture`
recorder, which does not report levels.

A recording belongs to the process that started it; `record_status` and
`record_stop` act on that process's own engine. While one runs, the Bugle
menu-bar app on the same Mac (same config) shows it in the menu bar within
a second or two, whichever process started it (`bugle serve`, `bugle mcp`,
`bugle record`), but cannot stop another process's recording: its Stop entry
is disabled and names the recording process. Stop it with `record_stop` on the
server that started it. Once a recording stops capturing by itself (its
`max_seconds`, a GIF cap, or a failure), the menu bar stops showing it even before that
`record_stop`, which still returns its media record.

When the ScreenCaptureKit recorder stops capturing early, `record_status`
turns `finishing` and carries `failure`, a message saying why, and capture
(microphone included) ends at that moment. What `record_stop` returns then
depends on the cause:

* The capture stream stopped (screen recording permission revoked, the
  recorded display unplugged or the window closed, or the user stopping it
  from the system's screen-sharing control): the recording is saved up to
  that point, and the media record carries the same `failure` (also when the
  menu-bar app already finalised it). With nothing captured, it fails with
  "no frames were captured (<why>)".
* Writing the file failed: nothing is kept, and `record_stop` fails with
  "recording failed: <cause>".

`source` accepts a media id (or prefix), `latest`, or a file path.
Target coordinates are screen points (top-left origin); annotation and OCR
coordinates are image pixels.

`target.kind` is one of `screen`, `window`, `area`, `interactive` (the user
drags a rectangle), `pick_window` and `all_screens`. `pick_window` (for
`capture` and `record_start`) asks the user to click a window in the menu-bar
app's hover-highlight picker, for when the agent cannot tell which window is
meant. It works only when the Bugle menu-bar app serves the API (*Start Agent
API* in its menu, or `bugle-app --serve`); `bugle serve` and `bugle mcp`
answer with an error naming that, HTTP status 503. (The CLI run in a terminal,
`bugle capture pick-window`, shows the picker itself.) The usual `Host` and token
checks run first: with a token configured, a client without it never gets the
overlay on screen; with the default (no token, loopback bind) any local
process that can reach the port can raise it, as it can call every other
action. The picker closes after 30 seconds; a cancel or timeout is HTTP 409
(`selection cancelled`). Requests are not queued: while another pick or area
selection is open, or for 3 seconds after the user dismissed a picker, a
`pick_window` request is refused with HTTP 409 and an error starting with
`busy:`; retry later. Abandoning the HTTP request does not close a picker
that is already up (the user, or its 30-second timeout, does). Space does not
switch to the area selection for API requests: the result is always a window.
`pick_window` works whatever `[screenshots] window_picker` says, since the
caller asked for it. A `delay` counts from the click, not from the request.
