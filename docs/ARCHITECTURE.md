# Architecture

Bugle is a Rust workspace of eight crates, split by subsystem so every feature has
an obvious home.

```
   ┌──────────────┐   ┌──────────────┐   ┌──────────────┐
   │  bugle-app   │   │  bugle-cli   │   │  bugle-api   │  HTTP (axum) + MCP (stdio)
   │ (menu bar)   │   │              │   │              │  (also embedded in bugle-app)
   └──────┬───────┘   └──────┬───────┘   └──────┬───────┘
          └──────────────────┼──────────────────┘
                     ┌──────▼───────┐
                     │ bugle-engine │  Engine: capture, scroll, record, annotate, OCR, AI
                     └┬─────┬─────┬┘
          ┌───────────▼┐  ┌─▼────────┐ ┌▼───────────┐
          │bugle-platform│ │bugle-markup│ │ bugle-ai  │
          │ (ObjC shim) │  │ (renderer) │ │(providers)│
          └───────────┬┘  └─┬────────┘ └┬───────────┘
                      └──────▼───────────▼┘
                          bugle-core  (types, config, history)
```

## Crate by crate

### `bugle-core`
Pure data: `Rect`/`Point`, `CaptureTarget`, `CaptureOptions`, `OcrResult`,
`Transcript` (with SRT/VTT rendering), `MediaItem`/`MediaKind`, the TOML `Config`
(including `SyncConfig`: folder *or* S3 destination, copy/move, kinds), filename
templates (`naming`) and the JSON-lines `History`. A history line whose id is
already present replaces the earlier record, which is how a moved file or a new
sync URL is stored without rewriting the file.


### `bugle-platform`
A C ABI over an Objective-C shim (`shim/shim.m`, compiled by `build.rs` with the
`cc` crate) plus safe Rust wrappers.

| Function | Framework call |
|---|---|
| `capture_display/area/window` | `SCShareableContent` + `SCScreenshotManager`, fallback `CGDisplayCreateImage` / `CGWindowListCreateImage` |
| `windows`, `displays` | `CGWindowListCopyWindowInfo`, `CGGetActiveDisplayList` |
| `ocr` | `VNRecognizeTextRequest` (accurate, `usesLanguageCorrection`, `recognitionLanguages`, auto-detect) |
| `register_translation` | `VNTranslationalImageRegistrationRequest` via `VNSequenceRequestHandler` |
| `post_scroll`, `cursor_position` | `CGEventCreateScrollWheelEvent`, `CGEventPost` |
| `select_rect_blocking`, `select_rect_begin`/`_cancel` | borderless `NSWindow` per screen, dim fill, crosshair, size label; blocking (`[NSApp run]`) or callback-based for a host with its own run loop |
| `window_pick_begin`/`_highlight`/`_cancel` (`shim/picker.m`) | see-through `NSWindow` per screen with an always-active `NSTrackingArea`; reports pointer moves and the final click/Esc/timeout through a C callback and draws whatever rect the host hands back |
| `pin_image_blocking`, `pin_image_show` | floating borderless window, drag/resize/close; blocking or fire-and-forget |
| `toast_show` | non-activating `NSPanel` with `NSVisualEffectView`, thumbnail + two lines, fades out |
| `clipboard_*` | `NSPasteboard` |
| `SckRecorder` (`bugle_sck_record_*`) | `SCStream` (`capturesAudio`, `excludesCurrentProcessAudio`) → `AVAssetWriter` H.264 + AAC .mp4; `AVCaptureSession` microphone as a second AAC track |
| `export_audio_m4a`, `export_mp4`, `export_mov`, `video_frame`, `media_duration` | `AVAssetExportSession`, `AVAssetImageGenerator` |
| `screen_access_*`, `accessibility_trusted` | `CGPreflightScreenCaptureAccess`, `AXIsProcessTrusted` |

Window shadows: ScreenCaptureKit clips single-window screenshots to the window
bounds, so the shadow path uses `CGWindowListCreateImage` (deprecated but
functional on macOS 26).

Both AppKit overlays must run on the main thread. The CLI calls the blocking
variants directly (each spins `[NSApp run]` and stops it with `[NSApp stop:]`);
the stand-alone servers spawn `bugle select --json` / `bugle pin` helper
processes; `bugle-app` installs an engine `UiHost` that marshals requests from
worker threads onto its already-running loop and uses the `_begin`/`_show`
variants, which never nest a run loop.

### `bugle-markup`
`Annotation` documents (serde) and a renderer. Shapes are rasterised from signed
distance functions so strokes are anti-aliased without a vector library; text uses
`ab_glyph` with a system font (Arial Bold → Helvetica → Geneva).


### `bugle-ai`
`Registry` of named `Provider`s built from `[ai.providers.*]`. Each provider is one
HTTP endpoint (or a local command) and implements the roles its kind supports:

| kind | vision | text | transcribe | wire format |
|---|---|---|---|---|
| `openai` | ✓ |
| `ollama` | ✓ |
| `anthropic` | ✓ |
| `whisper_cpp` |  |
| `command` | ✓ |

Adding a provider kind means implementing one of the three traits
(`VisionModel`, `TextModel`, `Transcriber`) in `providers.rs`.

### `bugle-engine`
The orchestration layer. `Engine` owns the config, history, AI registry and the
recorder state machine, and exposes one method per user-facing action:

* `capture` — delay → `grab` (per target) → optional Retina downscale → encode →
  history → clipboard.
* `scroll_capture` — grab, post scroll, settle, grab, measure shift (Vision
  registration; row-profile correlation fallback for sticky headers), append the
  new strip, repeat until the frame stops changing.
* `record_start/stop` — GIF: a thread grabbing frames at N fps, encoded on stop.
  Video: `screencapture -v` as a child process, stopped with SIGINT, remuxed to MP4
  with `AVAssetExportSession` passthrough — or, when system audio is requested
  (`recordings.backend = "auto"`) or always (`"stream"`), the in-process
  ScreenCaptureKit recorder: one `SCStream` delivers screen and system-audio
  sample buffers, `AVAssetWriter` encodes them to H.264 + AAC straight into the
  `.mp4` (a `.mov` request is remuxed afterwards). The microphone is an
  `AVCaptureSession` audio tap written as a second AAC track, retimed to the host
  clock; `export_audio_m4a` mixes both tracks when transcribing. Click highlighting
  is only available with the system recorder (`"system"`). Timing details live in the shim's
  recorder comment: the writer session opens before the stream starts, a static
  screen's stale first-frame timestamp is clamped, and the last frame is
  re-appended at the stop time so the video track covers the whole recording.
* `ocr` (Apple Vision or an AI provider, per request or `[ocr] engine`), `describe` (images or sampled video frames), `transcribe`
  (extracts the audio track first), `subtitles`, `annotate`, `pin`, `select_area`.
* `sync` (`sync.rs`) — runs after every successful save (`save_image`, so
  screenshots, scrolling captures and annotation output; `record_stop`) for the
  kinds listed in `[sync]`, before the item is written to history. A folder
  destination copies or moves the file (unique name on collision; a move updates
  the item's path). An S3 destination reads the credentials from the named
  environment variables, hand-rolls an AWS SigV4 `Authorization` header with
  `ring` (HMAC-SHA256, verified against the AWS test vectors) and `PUT`s the
  file path-style through `reqwest` on a throw-away single-thread runtime, so it
  works from the CLI's runtime, the servers' blocking pool and the app's worker
  thread alike. The object URL lands in `MediaItem.url` and on the clipboard when
  `general.copy_to_clipboard` is off. A failed sync is logged and never fails the
  capture; `sync_media` (CLI `bugle sync`, action `sync`, menu *Sync Latest*)
  syncs on demand regardless of `enabled`/`kinds` and updates history.

### `bugle-api`
`actions::catalog()` is the single list of tools with JSON schemas;
`actions::run()` dispatches by name. `http.rs` maps them to `POST /v1/actions/{name}`
plus REST aliases and serves `/v1/openapi.json`; `mcp.rs` is a newline-delimited
JSON-RPC server implementing `initialize`, `ping`, `tools/list`, `tools/call`.

### `bugle-cli`
Clap front end. Runs the tokio runtime on the main thread so AppKit overlays work.

### `bugle-app`
The menu-bar app. `tao` owns the main thread and the AppKit run loop (activation
policy *accessory*, so no Dock icon); `tray-icon`/`muda` build the `NSStatusItem`
and its `NSMenu`; `global-hotkey` registers Carbon `RegisterEventHotKey` bindings
from `[hotkeys]`. One table (`commands::MENU`) lists every menu row; the menu is
built by walking it and `[hotkeys]` keys are resolved against its names, so every
item is bindable and the two cannot drift. Unknown keys, unparsable or duplicate
combinations and bindings the OS refuses are logged, summarised in one toast and
skipped. Menu and hotkey events are forwarded to the loop as user events.
*Capture Window* opens the window picker (`picker.rs`): the main thread snapshots
the on-screen window list, shows the shim's overlay and, on every pointer move,
asks `window_at` — a pure function over the z-ordered list that skips Bugle's
own windows, non-zero layers, fully transparent (alpha 0), off-screen and
sub-50-point windows — which window to highlight. A click captures that window
by id after a 150 ms settle; Esc, a right click, Bugle losing active status (Esc
only arrives while it is active) or the 30-second timeout cancel. A request the
main thread reaches more than 3 s late is refused, and every refusal reaches
the worker with its reason. The picker and the area selection refuse to open
over each other. `[screenshots] window_picker = false` restores the immediate
front-most capture, which the CLI and API always use.
Engine actions run on one worker thread so the menu never blocks; when an action
needs the crosshair overlay or a pin window, the engine's `UiHost` (implemented
here) sends a request to the main thread and blocks the worker on a channel until
the overlay calls back. The agent API can be started from the menu on a tokio
runtime thread (`http::serve_with_shutdown`) and shares the same `Engine`, so API
callers get the in-app overlay too. Feedback is a transient toast (`toast_show`)
plus the shutter sound; the status item title shows `● m:ss` while recording.

## Feature ideas

Bugle is a standalone app; this is the backlog, roughly in the order it would
earn its keep.

* **Post-capture video editor** — trim, auto-zoom on clicks, cursor smoothing,
  export presets.
* **Audio mixing** — fold the microphone and system-audio tracks into one when
  asked; click highlighting on the ScreenCaptureKit recorder.
* **Quick-access panel** after a capture (the toast with a thumbnail is the
  minimal version): annotate, copy, pin or send from one place.
* **Window picker extras** — Space to switch to area selection, a picker for
  window recordings, and offering the picker to API callers through `UiHost`.
* **Packaging** — a signed `.app` bundle so notifications, Login Items and the
  permission prompts carry Bugle's own name.
* **Sync extras** — a destination per media kind, streaming multipart uploads
  for long recordings, and syncing sidecar subtitles next to their video.
