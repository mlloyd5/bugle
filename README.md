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

Cloud upload / link sharing is intentionally out of scope.

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
| System audio in recordings | needs an `SCStream` audio tap | ❌ not yet |
| Describe image / video frames with a vision model | OpenAI-compatible, Ollama, Anthropic, command | ✅ |
| Transcribe / subtitles (SRT, VTT) | whisper.cpp server, OpenAI-compatible, command | ✅ |
| History | JSON-lines file, ids, delete | ✅ |
| HTTP API + OpenAPI | axum, bearer token | ✅ |
| MCP server (stdio) | JSON-RPC 2.0, image content | ✅ |
| Menu-bar app, global hotkeys, Studio-mode video editor | — | ❌ future |

## Build

```bash
cargo build --release
```

The binary is `target/release/bugle`. The platform crate compiles a small
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

## Agents

See [docs/AGENTS.md](docs/AGENTS.md). In short:

```bash
# Claude Code
claude mcp add bugle -- /path/to/bugle mcp

# HTTP
bugle serve --token secret
curl -H 'Authorization: Bearer secret' -H 'content-type: application/json' \
     -d '{"target":{"kind":"window","owner":"Safari"}}' http://127.0.0.1:4848/v1/capture
```

Both transports expose the same 22 tools (`capture`, `list_windows`, `ocr`,
`describe`, `annotate`, `scroll_capture`, `record_start/stop/status`, `transcribe`,
`subtitles`, `history`, `pin`, …). `capture` returns the PNG inline so an agent can
look at what it took.

## Architecture

Seven crates; see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the details.

```
bugle-core      types, config, naming, history (no OS calls)
bugle-platform  Objective-C shim: SCKit/CG capture, Vision OCR + registration,
                scroll events, clipboard, selection overlay, pin panel, AVFoundation
bugle-annotate  annotation document model + SDF-based raster renderer
bugle-ai        provider registry: openai | ollama | anthropic | whisper_cpp | command
bugle-engine    Engine: capture, scroll stitch, record, annotate, OCR, AI actions
bugle-api       action catalog → HTTP (axum) and MCP (stdio)
bugle-cli       the `bugle` binary
```

## Why "Bugle"?

The Daily Bugle is the paper that *always* needs another picture of Spider-Man.
`jameson` (the editor who demands them) serves the HTTP API; `parker` (the one who
actually takes them) is the MCP server.
