# Bugle

> *"Pictures! I need pictures of Spider-Man!"* — J. Jonah Jameson, editor, the Daily Bugle

Bugle is a macOS screenshot and screen-recording tool written in Rust. It covers the
essentials — screen/window/area capture, scrolling capture, OCR, annotation,
pin-to-screen, GIF and video recording, history — and adds two things most
screenshot apps lack:

* **Pluggable AI providers** (local llama.cpp / Ollama / whisper.cpp servers, or
  OpenAI / Anthropic APIs) for describing screenshots, AI-assisted OCR and
  generating subtitles for recordings.
* **An agent API** — the same actions over HTTP (`bugle serve`, alias `jameson`)
  and MCP (`bugle mcp`, alias `parker`) so coding agents can take and read
  screenshots, annotate them and drive recordings.

There is no hosted sharing service and none is planned. Captures can instead be
[synced](#sync) to storage you own: a folder Dropbox or iCloud Drive mirrors, or
an S3-compatible bucket.

## Status

Verified on macOS 26.5 (arm64), Rust 1.99:

| Feature | Backend | State |
|---|---|---|
| Screen / area / window capture | ScreenCaptureKit (`SCScreenshotManager`), CoreGraphics fallback | ✅ |
| Window shadow | `CGWindowListCreateImage` | ✅ |
| Interactive area selection | AppKit overlay (dimmed screen, crosshair, size label) | ✅ |
| OCR with bounding boxes | Apple Vision `VNRecognizeTextRequest` (accurate, language correction) | ✅ |
| Scrolling capture | Vision `VNTranslationalImageRegistrationRequest` + row-profile fallback | ✅ (needs Accessibility) |
| Annotation | Pure-Rust renderer: arrows (4 styles), boxes, ellipses, lines, freehand, text, highlight, counters, pixelate/blur/black-out/secure-blur, spotlight, crop, padding + gradient background + shadow | ✅ |
| Pin to screen | Floating AppKit panel (drag, scroll-to-resize, Esc/double-click closes) | ✅ |
| GIF recording | In-process frame grab loop + `image` GIF encoder | ✅ |
| Video recording | `screencapture -v` (H.264 .mov → remuxed .mp4), microphone, click highlight | ✅ |
| System audio in recordings | ScreenCaptureKit `SCStream` audio tap + `AVAssetWriter` (H.264 + AAC .mp4), microphone as a second track | ✅ (macOS 14+) |
| Describe image / video frames with a vision model | OpenAI-compatible, Ollama, Anthropic, command | ✅ |
| Transcribe / subtitles (SRT, VTT) | whisper.cpp server, OpenAI-compatible, command | ✅ |
| History | JSON-lines file, ids, delete | ✅ |
| HTTP API + OpenAPI | axum, bearer token | ✅ |
| MCP server (stdio) | JSON-RPC 2.0, image content | ✅ |
| Menu-bar app, global hotkeys | `bugle-app`: `NSStatusItem` via `tray-icon`, Carbon hotkeys via `global-hotkey`, in-app overlays | ✅ |
| Sync to your own storage | folder copy/move, or SigV4 uploads (multipart for large files) to any S3-compatible bucket; a destination per media kind; subtitles follow their video | ✅ |
| Post-capture video editor | — | see [feature ideas](docs/ARCHITECTURE.md#feature-ideas) |

## Build

```bash
cargo build --release
```

The binaries are `target/release/bugle` (CLI + servers) and `target/release/bugle-app`
(menu-bar app). The platform crate compiles a small
Objective-C shim (`crates/bugle-platform/shim/shim.m`) with the Xcode command
line tools; no Xcode project is needed.

## Permissions

* **Screen Recording** — required for any capture. The first call triggers the
  system prompt; grant it to the app that launched `bugle` (Terminal, iTerm, the
  Claude app, …). `bugle permissions --request` asks explicitly.
* **Accessibility** — required only for scrolling capture (it posts scroll-wheel
  events).

## CLI

```bash
bugle capture                      # drag an area (TTY) / main screen (non-TTY)
bugle capture screen --display 3
bugle capture window --app Safari --no-shadow -c       # -c copies to clipboard
bugle capture area --rect 100,100,800,600 -f jpeg -q 85 -o ~/Desktop/x.jpg
bugle capture window --title "Lab power" --ocr --describe --pin
bugle capture all                  # every display, stitched

bugle scroll                       # drag the scrolling area, then keep the mouse over it
bugle scroll --rect 0,120,1200,800 --max-frames 60 --step -10

bugle record start area --rect 0,30,1280,720 -f gif --fps 10 -s 8
bugle record start window --app Xcode -f mp4 --mic --subtitles   # Ctrl-C to stop
bugle record start screen -f mp4 --system-audio -s 30              # ScreenCaptureKit backend

bugle ocr latest                   # Apple Vision, reading order, --json for boxes
bugle ocr latest --engine llama    # OCR through a vision model instead
bugle describe latest --with-ocr --provider ollama
bugle describe <video-id>          # samples 4 frames and describes the recording
bugle transcribe latest
bugle subtitles latest -f vtt --provider whisper

bugle annotate latest --arrow 200,300,500,150 --box 100,100,400,200 \
    --text "120,60:Look here" --counter 150,400 --redact 600,500,300,100 \
    --canvas sunset --padding 60 --open
bugle annotate latest --spec annotations.json      # full document, see below
bugle annotate --schema                            # example of every shape

bugle pin latest --at 1900,120,600,300 --opacity 0.95
bugle windows | bugle displays | bugle history -n 10 | bugle delete <id> --file
bugle sync latest                  # copy/move/upload to the [sync] destination
bugle providers | bugle ask "summarise this OCR output: ..."
bugle config init | bugle config show
bugle serve --token secret         # HTTP API on 127.0.0.1:4848   (alias: jameson)
bugle mcp                          # MCP over stdio                (alias: parker)
```

`latest` (or a media id prefix, or a file path) is accepted anywhere a source is.
Every command takes `--json`.

### Annotation documents

Coordinates are image pixels. Styles can be set per shape or document-wide.

```json
{
  "crop": {"x": 0, "y": 0, "w": 800, "h": 500},
  "style": {"color": "blue", "stroke_width": 5},
  "shapes": [
    {"type": "ellipse",  "rect": {"x": 50, "y": 50, "w": 200, "h": 120}, "fill": "#00FF0040"},
    {"type": "arrow",    "from": {"x": 300, "y": 400}, "to": {"x": 150, "y": 180}, "arrow": "fancy", "color": "red"},
    {"type": "text",     "at": {"x": 320, "y": 420}, "text": "Fancy arrow", "background": "#000000B0", "color": "white"},
    {"type": "counter",  "at": {"x": 100, "y": 100}},
    {"type": "redact",   "rect": {"x": 500, "y": 300, "w": 200, "h": 80}, "mode": "secure_blur"},
    {"type": "spotlight","rect": {"x": 40, "y": 40, "w": 300, "h": 200}, "dim": 0.5}
  ],
  "canvas": {"padding": 40, "background": "gradient:#1e3c72,#2a5298", "corner_radius": 16, "shadow": true},
  "scale": 1.0
}
```

Shapes: `rectangle`, `ellipse`, `line`, `arrow` (standard/double/slim/fancy),
`freehand`, `text`, `highlight`, `counter`, `redact` (pixelate/blur/black_out/secure_blur),
`spotlight`, `image`. Background presets: sunset, ocean, forest, candy, mono, midnight,
peach, mint, any `#hex`, or `gradient:#a,#b[,angle]`.

## Menu-bar app

```bash
cargo run -p bugle-app            # or target/release/bugle-app [--serve] [--config FILE]
```

`bugle-app` is an accessory process (no Dock icon) that puts a camera glyph in the
menu bar and listens for global hotkeys. Everything it does goes through the same
`Engine` as the CLI, so captures land in the configured `save_dir`, appear in
history, and play the shutter sound when `general.shutter_sound` is on.

Menu: Capture Area · Capture Window (front-most window) · Capture Screen ·
Scrolling Capture · Record Video · Record GIF · Stop Recording · OCR Latest
(copies the text) · Pin Latest · Recent Captures (last five, click to open) ·
Open History Folder · Start/Stop Agent API · Preferences… (opens the config
file) · Quit. While recording, the status item shows `● m:ss`. After each
action a small toast in the top-right corner shows a thumbnail and the file
name; clicking it opens the file.

Hotkeys live in the `[hotkeys]` section of the config file and are parsed as
`modifier+modifier+key` (`cmd`, `shift`, `alt`/`option`, `ctrl`; letters,
digits, `F1`…`F24`, `space`, …). Defaults:

| Action | Key | Default |
|---|---|---|
| `capture_area` | ⌘⇧4 | drag an area with the crosshair overlay |
| `capture_screen` | ⌘⇧3 | main display |
| `record` | ⌘⇧5 | start an MP4 recording of a dragged area, or stop the active one |
| `ocr_latest` | ⌘⇧2 | OCR the latest capture to the clipboard |
| `pin_latest` | ⌘⇧P | pin the latest capture |
| `capture_window`, `scroll_capture`, `record_gif` | — | unbound until set |

An empty string unbinds an action. ⌘⇧3/4/5 are also the system screenshot
shortcuts; disable those in System Settings → Keyboard → Keyboard Shortcuts →
Screenshots (or pick other keys) so they do not fire twice. The agent API started
from the menu uses `[api]` from the config and serves the in-app overlay for
`select_area`, `pin` and interactive captures — no helper processes.

Screen Recording permission is requested on first launch (granted to the
terminal or app that launched `bugle-app` when run from a shell).

## Configuration

`~/.config/bugle/config.toml` (or `$BUGLE_CONFIG`). See
[`config.example.toml`](config.example.toml) for every key. Run `bugle config init`
to write the defaults, which point at the usual local servers:

| Provider | Kind | Default endpoint |
|---|---|---|
| `llama` | openai | `http://127.0.0.1:8080/v1` (llama.cpp `llama-server`) |
| `ollama` | ollama | `http://127.0.0.1:11434` |
| `whisper` | whisper_cpp | `http://127.0.0.1:8178` (whisper.cpp `whisper-server`) |
| `openai` | openai | `https://api.openai.com/v1` (`OPENAI_API_KEY`) |
| `anthropic` | anthropic | `https://api.anthropic.com` (`ANTHROPIC_API_KEY`) |

Any OpenAI-compatible server works for `kind = "openai"` (LM Studio, vLLM,
OpenRouter, Groq…). `kind = "command"` wraps a local executable, e.g. `whisper-cli`.

The other sections: `[general]` (save folder, filename template, clipboard),
`[screenshots]`, `[recordings]` (backend, audio), `[ocr]`, `[markup]` defaults,
`[api]` (bind address, token, CORS), `[hotkeys]` for the menu-bar app and `[sync]`
below.

The HTTP API is meant for local agents. Setting a token is recommended; without
one any local process can call it. Binding to a non-loopback address (e.g.
`--bind 0.0.0.0:4848`) is refused unless a token is set, a loopback listener
rejects requests whose `Host` is not `127.0.0.1`, `localhost` or `[::1]`, and
`cors = true` only takes effect together with a token.

### Sync

`[sync]` sends every new capture to storage you own — no accounts, no hosted
service. Pick one destination:

```toml
[sync]
enabled = true
mode = "copy"                 # or "move": the file (and its history entry) relocates
kinds = ["screenshot", "scroll_capture", "annotated", "video", "gif"]
folder = "~/Library/Mobile Documents/com~apple~CloudDocs/Bugle"   # iCloud Drive, or ~/Dropbox/…
```

or an S3-compatible bucket (AWS S3, MinIO, Garage, Cloudflare R2, Backblaze B2…):

```toml
[sync]
enabled = true
[sync.s3]
endpoint = "https://s3.eu-west-1.amazonaws.com"
bucket = "my-captures"
region = "eu-west-1"
prefix = "bugle"
access_key_env = "AWS_ACCESS_KEY_ID"      # credentials are read from the environment
secret_key_env = "AWS_SECRET_ACCESS_KEY"
# public_url = "https://captures.example.com"
```

Uploads are SigV4-signed (path-style, no SDK) and stream from disk; files of
`multipart_threshold_mb` (64, at most 5120) or more go up as a multipart upload
in `part_size_mb` (16, 5..=5120) parts. Each request is retried up to six times
with a capped, jittered backoff (and the server's `Retry-After`), and the upload
is aborted if it cannot finish or the file changes meanwhile. An upload cut
short by a crash, or a create request whose response was lost, can leave an
incomplete upload behind: add a lifecycle rule to the bucket that aborts
incomplete multipart uploads after a day. The sync runs right after the save,
on the same thread: the capture is in history at once, but `record_stop` (and
the menu-bar action) returns when the upload has finished. The object URL is stored
in the history item (`url`) and copied to the clipboard unless
`general.copy_to_clipboard` already puts the image there. `bugle sync latest`,
the `sync` agent action and *Sync Latest* in the menu bar do the same on demand,
even with `enabled = false`.

Automatic sync covers what Bugle saves: screenshots, scrolling captures,
annotations, recordings and GIFs (as listed in `kinds`), and subtitles written by
`bugle subtitles`. Audio and text files are synced on demand with `bugle sync`.
Each media kind can have its own destination; kinds without one use the default,
and a kind with its own entry syncs automatically when it is saved:

```toml
[sync.destinations.video]
prefix = "recordings"            # sub-folder / key prefix under the default destination
[sync.destinations.gif]
folder = "~/Dropbox/GIFs"        # or a [sync.destinations.gif.s3] table
mode = "move"
```

With `sidecars = true` (the default), a video's `.srt` / `.vtt` subtitles are
synced to the video's destination (or to `[sync.destinations.subtitle]` when
that is set) under the video's synced name (`clip 2.mp4` → `clip 2.srt`),
including subtitles generated after the video was synced. A file there that
Bugle did not write for that video is never replaced: the subtitle gets a unique
name instead. A subtitle written to an explicit `--output` is copied even in
move mode. `prefix` must be relative and free of `.` / `..` segments.

## Agents

See [docs/AGENTS.md](docs/AGENTS.md). In short:

```bash
# Claude Code
claude mcp add bugle -- /path/to/bugle mcp

# HTTP
bugle serve --token secret         # token recommended; required off loopback
curl -H 'Authorization: Bearer secret' -H 'content-type: application/json' \
     -d '{"target":{"kind":"window","owner":"Safari"}}' http://127.0.0.1:4848/v1/capture
```

Both transports expose the same 23 tools (`capture`, `list_windows`, `ocr`,
`describe`, `annotate`, `scroll_capture`, `record_start/stop/status`, `transcribe`,
`subtitles`, `history`, `pin`, `sync`, …). `capture` returns the PNG inline so an agent can
look at what it took.

## Architecture

Eight crates; see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the details.

```
bugle-core      types, config, naming, history (no OS calls)
bugle-platform  Objective-C shim: SCKit/CG capture, Vision OCR + registration,
                scroll events, clipboard, selection overlay, pin panel, AVFoundation
bugle-markup    annotation document model + SDF-based raster renderer
bugle-ai        provider registry: openai | ollama | anthropic | whisper_cpp | command
bugle-engine    Engine: capture, scroll stitch, record, annotate, OCR, AI actions
bugle-api       action catalog → HTTP (axum) and MCP (stdio)
bugle-cli       the `bugle` binary
bugle-app       the `bugle-app` menu-bar binary: status item, hotkeys, toasts
```

## Why "Bugle"?

The Daily Bugle is the paper that *always* needs another picture of Spider-Man.
`jameson` (the editor who demands them) serves the HTTP API; `parker` (the one who
actually takes them) is the MCP server.
