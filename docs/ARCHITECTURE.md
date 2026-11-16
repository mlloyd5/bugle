# Architecture

Bugle is a Rust workspace of eight crates (plus `xtask` for packaging), split by subsystem so every feature has
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
(including `SyncConfig`: folder *or* S3 destination, copy/move, kinds, per-kind
`destinations`), filename
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
| `window_pick_begin`/`_highlight`/`_cancel`/`_switch` (`shim/picker.m`) | see-through `NSWindow` per screen with an always-active `NSTrackingArea`; reports pointer moves and the final click/Esc/Space/timeout through a C callback and draws whatever rect the host hands back |
| `select_rect_begin_ex`, `select_rect_switch` | the selection overlay with an optional timeout and a Space "switch" result (not while dragging); a switch from either overlay leaves Bugle active so the other can open at once |
| `pin_image_blocking`, `pin_image_show` | floating borderless window, drag/resize/close; blocking or fire-and-forget |
| `toast_show` | non-activating `NSPanel` with `NSVisualEffectView`, thumbnail + two lines, fades out |
| `editor_open` (`shim/editor.m`) | markup editor window: toolbar, canvas scaled to fit, label text field, unsaved-changes sheet; reports input in image pixels through a callback and draws the image and overlay primitives it is handed |
| `panel_show` (`shim/panel.m`) | quick-access panel: non-activating, never-key `NSPanel`, thumbnail, buttons, close; reports clicks through a callback |
| `clipboard_*` | `NSPasteboard` |
| `SckRecorder` (`bugle_sck_record_*`) | `SCStream` (`capturesAudio`, `excludesCurrentProcessAudio`, `showMouseClicks`) → `AVAssetWriter` H.264 + AAC .mp4; `AVCaptureSession` microphone as a second AAC track |
| `export_audio_m4a`, `export_mp4`, `export_mov`, `video_frame`, `media_duration` | `AVAssetExportSession`, `AVAssetImageGenerator` |
| `mix_audio_tracks`, `audio_track_count`, `video_track_count` | `AVAssetReaderAudioMixOutput` (+ `AVMutableAudioMix` levels) → `AVAssetWriter` (AAC), video samples copied on their own timescale |
| `export_video`, `video_info`, `video_sync_at`, `VideoFrameReader`, `VideoFrameWriter` | `AVAssetExportSession` with `timeRange`/preset, an `AVMutableVideoComposition` on every re-encode (`frameDuration` + `sourceTrackIDForFrameTiming = kCMPersistentTrackID_Invalid` for a new frame rate), bounded wait + `cancelExport`; `AVSampleCursor` (stored samples, sync sample at a cut); one reusable `AVAssetImageGenerator`; `AVAssetWriter` + pixel-buffer adaptor (H.264 or HEVC) |
| `screen_access_*`, `accessibility_trusted` | `CGPreflightScreenCaptureAccess`, `AXIsProcessTrusted` |
| `login_item::{in_app_bundle, status, set_enabled}` | `SMAppService.mainAppService` (`shim/login_item.m`, ServiceManagement) |

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
  clock; `export_audio_m4a` mixes both tracks when transcribing. With
  `mix_audio` (`[recordings]`, `--mix-audio`, action argument) a post-pass on the
  finished file folds all audio tracks into one AAC track (a `.mov` request is
  mixed straight to QuickTime); mixing after capture keeps the real-time path
  unchanged (no cross-clock resampling or buffering between two live sources).
  Each of the n sources is scaled by 1/n so the sum cannot clip; the audio is
  re-encoded (a second AAC generation) while video samples and frame times are
  copied. The mix runs during stop, writes a uniquely named hidden copy next to
  the recording (so it needs free space equal to the recording) and replaces the
  file only after checking the copy's tracks and duration; a failed mix logs a
  warning and keeps the two tracks. Asking for it without both sources on the
  stream recorder warns at start. Click highlighting
  uses `screencapture -k` or, on the stream recorder, `SCStreamConfiguration.showMouseClicks`
  (macOS 15+). Timing details live in the shim's
  recorder comment: the writer session opens before the stream starts, a static
  screen's stale first-frame timestamp is clamped, and the last frame is
  re-appended at the stop time so the video track covers the whole recording.
* `edit` (`edit.rs`) — trims a recording (`start`/`end` seconds, negative from
  the end, measured on the video track) and exports it with a preset. Video →
  video is one `AVAssetExportSession` over the time range, audio kept.
  `original` copies samples only when the cut starts on a sync sample (checked
  with an `AVSampleCursor`); otherwise it re-encodes, because a copied cut
  between keyframes keeps the frames back to the previous keyframe in the file
  behind an edit list. `lossless` always copies and says so. Every re-encode
  renders through a video composition, which stores only frames inside the
  range (an export without one also starts at the earlier keyframe) and is how
  `fps` resamples. `high`/`hevc` keep the source size, `1080p`/`720p`/`540p`
  fit a box (never upscaling). Exports are cancelled after 120 s + 10 s per
  second of output, and an engine runs at most two edits at once. Video → GIF
  decodes frames exactly (zero tolerance, so the cut never bleeds to a keyframe
  outside it) in the middle of each output frame interval, holding the last
  frame if the track ends early. GIF sources are streamed twice (timeline, then
  frames) under decoder size/allocation limits, never held whole: GIF → GIF is
  a frame-exact cut (edge delays shortened) unless a rate is given; GIF → video
  resamples to a constant rate for `AVAssetWriter` (HEVC for `hevc`). GIF delays
  are written in centiseconds with the rounding error carried forward (minimum
  2 cs), and the recorded duration is what was written. The `gif` preset uses
  `recordings.gif_frame_rate` for video sources and keeps a GIF source's timing;
  both are scaled to `gif_max_size`. Output is written to a temp file beside the
  target (extension corrected to the container) and renamed, then added to
  history, with the source as `parent` when it is a history item.
* `ocr` (Apple Vision or an AI provider, per request or `[ocr] engine`), `describe` (images or sampled video frames), `transcribe`
  (extracts the audio track first), `subtitles`, `annotate`, `pin`, `select_area`.
* `sync` (`sync.rs`) — runs after every successful save (`save_image`, so
  screenshots, scrolling captures and annotation output; `record_stop`) for the
  kinds listed in `[sync]` (or given their own `[sync.destinations.<kind>]`),
  right after the item is written to history (which the step then updates), on
  the caller's thread. The destination and mode are resolved
  per kind, falling back to the default. A folder destination copies or moves
  the file (unique name on collision; a move updates the item's path). An S3
  destination reads the credentials from the named environment variables,
  hand-rolls an AWS SigV4 `Authorization` header with `ring` (HMAC-SHA256,
  verified against the AWS test vectors) and talks path-style through `reqwest`
  on a throw-away single-thread runtime, so it works from the CLI's runtime, the
  servers' blocking pool and the app's worker thread alike. Bodies stream from
  disk in 256 KiB reads with a signed `Content-Length`: one `PUT` below
  `multipart_threshold_mb`, otherwise create / upload-part / complete, each
  request signed afresh, with a deadline scaled to its body, and retried with
  capped jittered backoff (5xx, 429, `RequestTimeout`, transport errors, a 200
  carrying `<Error>`; `Retry-After` honoured). A retried complete that meets
  `NoSuchUpload` checks the object with `HEAD`; a failure, or a file that changed
  size, aborts the upload. Error bodies are cut down to `Code` / `Message`. The
  name the copy got is kept in `MediaItem.synced_as`; a video's `.srt` / `.vtt`
  sidecars go to the video's destination (or the `subtitle` entry) under that
  stem (`clip 2.mp4` → `clip 2.srt`), replacing only a sidecar Bugle wrote for
  that video, and `subtitles` written later follow a synced video. Tests check
  the signer against the published AWS vectors and drive the S3 path against an
  in-process axum mock that checks signatures, part order and sizes, ETags and
  aborts. The object URL lands in `MediaItem.url` and on the clipboard when
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
*Capture Window* and *Record Window* open the window picker (`picker.rs`): the main thread snapshots
the on-screen window list, shows the shim's overlay and, on every pointer move,
asks `window_at` — a pure function over the z-ordered list that skips Bugle's
own windows, non-zero layers, fully transparent (alpha 0), off-screen and
sub-50-point windows — which window to highlight. A click captures that window
by id after a 150 ms settle; Esc, a right click, Bugle losing active status (Esc
only arrives while it is active) or the 30-second timeout cancel. A request the
main thread reaches more than 3 s late is refused, and every refusal reaches
the worker with its reason. The picker and the area selection refuse to open
over each other. Space switches between them: a pick *session* is a small
state machine (`picker::Session`, unit-tested without a screen) that starts in
either mode, flips on each switch and answers with a window or a rect; the
main thread opens the next overlay from the previous one's final callback.
*Capture Area*, *Capture Window* and the three *Record* items run through it;
each picker turn keeps the 30 s timeout and a whole session closes after two
minutes. `[screenshots] window_picker = false` restores the immediate
front-most capture (and plain area selection), which the CLI always uses.
`UiHost::pick_window` offers the picker (without the switch) to API callers:
the `pick_window` target kind of `capture` and `record_start` goes through it,
and an engine without a host refuses that target up front with
`BugleError::NoUiHost` (HTTP 503) before any permission prompt.
Engine actions run on one worker thread so the menu never blocks; when an action
needs the crosshair overlay or a pin window, the engine's `UiHost` (implemented
here) sends a request to the main thread and blocks the worker on a channel until
the overlay calls back. The agent API can be started from the menu on a tokio
runtime thread (`http::serve_with_shutdown`) and shares the same `Engine`, so API
callers get the in-app overlay too. Feedback is a transient toast (`toast_show`)
plus the shutter sound; the status item title shows `● m:ss` while recording.
When the process runs from `Bugle.app`, an *Open at Login* check item
(`login.rs`) registers the bundle with `SMAppService`. Its check mark is
re-read on the periodic tick (every 5 s) and set only for an enabled item; a
pending approval is shown in the title, and clicking it then opens System
Settings → Login Items.

The status-bar glyph is a signed distance field (`icon/glyph.rs`) that scales
its stroke widths with the render size, so the same drawing serves the 36 px
template image and the application icon.

### `xtask` (packaging)
`cargo xtask bundle` (alias in `.cargo/config.toml`) builds `bugle-app` and
`bugle` in release mode, taking the executable paths from
`cargo build --message-format=json` and the target directory from
`cargo metadata`, and assembles the bundle in `dist/.staging`. The bundle id
is read from `[workspace.metadata.bugle] bundle_id` in the root `Cargo.toml`
(its only definition; `--bundle-id` overrides it and is validated):

```
Bugle.app/Contents/
  Info.plist          generated (plist.rs): bundle id, workspace version, LSUIElement,
                      NSMicrophoneUsageDescription (Screen Recording and
                      Accessibility have no usage key)
  PkgInfo
  MacOS/bugle-app     CFBundleExecutable
  MacOS/bugle         the CLI, signed as <bundle id>.cli
  Resources/Bugle.icns  icns.rs: glyph.rs (included by path) on a gradient square,
                        rendered per size and written as PNG elements, reproducibly
```

Signing is inside-out (CLI, then the bundle) with `--options runtime` and
`packaging/Bugle.entitlements` (`device.audio-input`, which the hardened runtime
requires for the microphone); ad-hoc by default, a Developer ID identity from
`--sign`/`BUGLE_SIGN_IDENTITY` adds a secure timestamp. Verification runs
`plutil -lint` on Info.plist, `codesign --verify --deep --strict`, checks the
runtime flag and lints the entitlements embedded in both signatures (app and
CLI), round-trips the icon through `iconutil` and runs both binaries with
`--version`. `--notarize` needs a Developer ID identity and a keychain profile
or an App Store Connect API key file (never a password in argv) and fails up
front without them; it submits a temporary `ditto` zip to `notarytool`,
staples, and requires `spctl --assess` to pass. Only then is the staged bundle
renamed over `dist/Bugle.app` and the final zip written.

After a capture the app shows the quick-access panel instead of the toast
(`[quick_access]`, on by default). `quick_access.rs` is the platform-free part:
which buttons a media kind gets, the current panel generation (stale clicks are
ignored), the timeout (paused while hovered, checked on the loop tick) and
`perform`, which maps each button onto the engine through a `Backend` trait —
Copy → `Engine::copy_media`, Pin → `Engine::pin`, Sync → `Engine::sync_media`,
Annotate → the markup editor (or open in `annotate_app`), Reveal → Finder. `quick_panel.rs` is the glue:
it shows the panel, turns clicks into `AppEvent::QuickAccess`, runs blocking
actions on a short-lived thread (Pin must, since its `UiHost` request
round-trips through the main thread) and swaps in a sharper thumbnail rendered
off the main thread.

The markup editor follows the same split. `markup_editor.rs` is the
platform-free part: `Editor::handle` turns the window's events (pointer in
image pixels, tool, colour, size, commands, typed text) into edits of a
`Vec<Shape>` with snapshot undo, and answers with `Effects` saying what to
refresh. `editor_window.rs` is the glue: it renders `Editor::document` with
`Engine::render_annotations` and hands the pixels to the window, so the preview
is the renderer's output; only the shape being dragged (and the selection
marks) go to the window as overlay primitives, and a shape being moved is left
out of the render meanwhile. Save runs `Engine::apply_annotations` on a thread
and the window closes only once it succeeded; `shim/editor.m` frees the Rust
handler on a later main-loop turn, so closing from inside the callback is safe.
`--smoke-test` drives it through `editor_inject`.

## Feature ideas

Bugle is a standalone app; this is the backlog, roughly in the order it would
earn its keep.

* **Video editor extras** — auto-zoom on clicks and cursor smoothing (the
  recorders bake the cursor into the frames and log no click or cursor
  positions, so these first need a click/cursor event track recorded alongside
  the video), and an editing window on top of the headless `edit` pipeline.
* **Audio mixing extras** — per-source levels (or ducking system audio under
  the microphone) for the mixed track.
* **Markup editor extras** — crop and canvas (padding, background) in the
  window, multi-line labels, arrow and redaction styles, zoom, a preview
  render off the main thread for very large captures, and opening a second
  editor. Also: keep the quick-access panel's thumbnails stacked when several
  captures arrive in a row.
* **Window picker extras** — Space in the Scrolling Capture selection, and the
  picker for `bugle capture` in a terminal (it needs a run loop of its own).
* **Packaging extras** — a universal (arm64 + x86_64) build (bundles are
  host-architecture only today), a DMG, the final bundle id, and system
  notifications (`UNUserNotificationCenter`) alongside the toast now that the
  bundle gives them Bugle's name.
