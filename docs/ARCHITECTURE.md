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
sync URL is stored without rewriting the file. `audio_mix` holds the sample
math for the mixed recording track (per-source gain, ducking, peak limiter),
so it is tested on synthetic buffers without AVFoundation. `events` defines a recording's
pointer/click event track, the `<file>.events.json` sidecar named after the
whole file name (format in the module docs: recorded area in global points,
`move`/`down`/`up` events in points relative to it on the video's timeline,
`cursor_in_video`, `fps`, `area_changed`), with validation, interpolation and
trimming; `MediaItem.events` says where a history item's track is (an effects
edit has none), and `History::delete` removes it with the file.


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
| `mouse_buttons` | `CGEventSourceButtonState` (session state; no permission, nothing posted) |
| `host_time_now`, `SckRecorder::session_start`, `window_frame`, `child_exited` | `CMClockGetHostTimeClock`; the stream recorder's writer session start; `CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow)`; `waitid(WEXITED \| WNOHANG \| WNOWAIT)` (does not reap) |
| `SequentialFrameReader`, `mux_audio` | `AVAssetReader` over a time range (BGRA, presentation order); `AVMutableComposition` (rendered video + source audio range) → passthrough `AVAssetExportSession` |
| `select_rect_blocking`, `select_rect_begin`/`_cancel` | borderless `NSWindow` per screen, dim fill, crosshair, size label; blocking (`[NSApp run]`) or callback-based for a host with its own run loop |
| `window_pick_begin`/`_highlight`/`_cancel`/`_switch` (`shim/picker.m`) | see-through `NSWindow` per screen with an always-active `NSTrackingArea`; reports pointer moves and the final click/Esc/Space/timeout through a C callback and draws whatever rect the host hands back |
| `select_rect_begin_ex`, `select_rect_switch` | the selection overlay with an optional timeout and a Space "switch" result (not while dragging); a switch from either overlay leaves Bugle active so the other can open at once |
| `pin_image_blocking`, `pin_image_show` | floating borderless window, drag/resize/close; blocking or fire-and-forget |
| `toast_show` | non-activating `NSPanel` with `NSVisualEffectView`, thumbnail + two lines, fades out |
| `editor_open` (`shim/editor.m`) | markup editor window: toolbar with pull-down menus, canvas in an `NSScrollView` (fit, zoom stops, pinch, ⌘+/⌘−/⌘1/⌘0), multi-line label text field, unsaved-changes sheet; reports input in edited-image pixels through a callback and draws the image (told where the edited image's origin lies in it), overlay primitives and menus it is handed |
| `panel_show` (`shim/panel.m`) | quick-access panel: non-activating, never-key `NSPanel`, thumbnail, buttons, close; reports clicks through a callback |
| `clipboard_*` | `NSPasteboard` |
| `SckRecorder` (`bugle_sck_record_*`) | `SCStream` (`capturesAudio`, `excludesCurrentProcessAudio`, `showMouseClicks`) → `AVAssetWriter` H.264 + AAC .mp4; `AVCaptureSession` microphone as a second AAC track |
| `export_audio_m4a`, `export_mp4`, `export_mov`, `video_frame`, `media_duration` | `AVAssetExportSession`, `AVAssetImageGenerator` |
| `mix_audio_tracks(_with)`, `audio_track_count`, `video_track_count` | one `AVAssetReaderAudioMixOutput` per audio track (float PCM) → Rust mix callback (`bugle_core::audio_mix`) → `AVAssetWriter` (AAC), video samples copied on their own timescale |
| `export_video`, `video_info`, `video_sync_at`, `VideoFrameReader`, `VideoFrameWriter` | `AVAssetExportSession` with `timeRange`/preset, an `AVMutableVideoComposition` on every re-encode (`frameDuration` + `sourceTrackIDForFrameTiming = kCMPersistentTrackID_Invalid` for a new frame rate), bounded wait + `cancelExport`; `AVSampleCursor` (stored samples, sync sample at a cut); one reusable `AVAssetImageGenerator`; `AVAssetWriter` + pixel-buffer adaptor (H.264 or HEVC) |
| `screen_access_*`, `accessibility_trusted` | `CGPreflightScreenCaptureAccess`, `AXIsProcessTrusted` |
| `login_item::{in_app_bundle, status, set_enabled}` | `SMAppService.mainAppService` (`shim/login_item.m`, ServiceManagement) |
| `notify::{start, state, refresh, post}` | `UNUserNotificationCenter` (`shim/notify.m`, UserNotifications): delegate (banner while active, click opens the file), `requestAuthorizationWithOptions` or `getNotificationSettings`, `addNotificationRequest`; nothing is called outside an app bundle |

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
`ab_glyph` with a system font (Arial Bold → Helvetica → Geneva), one line per
`text_lines` entry (`\n`, `\r\n`, a lone `\r`, U+2028/U+2029). The canvas
shadow blurs a one-channel mask with three box blurs (running sums), so its
cost does not grow with the blur radius. `Shape::translate` and
`Annotation::source_origin` (where source pixel (0, 0) lands after crop and
canvas) let an editor keep shapes in original-image pixels.


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
  with `AVAssetExportSession` passthrough — or, when system audio, `hide_cursor`
  or `cursor_events` is requested (`recordings.backend = "auto"`) or always
  (`"stream"`), the in-process
  ScreenCaptureKit recorder: one `SCStream` delivers screen and system-audio
  sample buffers, `AVAssetWriter` encodes them to H.264 + AAC straight into the
  `.mp4` (a `.mov` request is remuxed afterwards). The microphone is an
  `AVCaptureSession` audio tap written as a second AAC track, retimed to the host
  clock; `export_audio_m4a` mixes both tracks when transcribing. With
  `mix_audio` (`[recordings]`, `--mix-audio`, action argument) a post-pass on the
  finished file folds all audio tracks into one AAC track (a `.mov` request is
  mixed straight to QuickTime); mixing after capture keeps the real-time path
  unchanged (no cross-clock resampling or buffering between two live sources).
  Each track is decoded to 48 kHz stereo float by its own reader and the
  tracks are lined up by presentation time (late starts and gaps become
  silence); `bugle_core::audio_mix::Mixer` then mixes each block. Each of the
  n sources gets a base share of 1/n, so at the default levels the sum cannot
  clip; `microphone_gain` and `system_audio_gain` are linear factors on those
  shares, 1.0 being the default share (track 0 is system audio, track 1 the
  microphone, the order the recorder writes them). With `ducking`, a peak
  detector on the microphone as recorded (before its gain; instant rise,
  60 ms release) decides when it is active, above `ducking_threshold_db`, and
  the system-audio gain follows towards `-ducking_amount_db` with
  `ducking_attack_ms` and back with `ducking_release_ms`; a microphone at gain
  0 never ducks. With every gain at 1 or below the sum is passed through
  untouched; with any gain above 1 a stereo-linked peak limiter (instant
  attack, 100 ms release, ceiling -0.3 dBFS, headroom because decoded AAC can
  overshoot slightly) catches what the gains push past full scale. Levels come
  from `[recordings]` with per-recording overrides (`--mic-gain`,
  `--system-gain`, `--duck` / `--no-duck`, `--duck-amount`; the same names as
  action arguments) and only apply to the mixed track. Out-of-range
  `[recordings]` levels are logged when the engine loads the config; starting
  a mixed recording with out-of-range levels (config or request) is an
  invalid request. The audio is
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
  With `cursor_events` (`event_log.rs`) a thread polls the pointer and the
  button state at 60 Hz from just before the recorder starts and logs changes,
  stamped on the host clock. On stop they are placed on the video's timeline
  by where its time 0 is: the stream recorder's writer session start (the
  same clock), the GIF loop's per-frame grab times (frame `k` shown at
  `k / fps`, so grabs that fall behind are accounted for), or for the system
  recorder (used for event tracks only with `backend = "system"` or without
  ScreenCaptureKit, since `auto` picks the stream recorder for them) an
  estimate: the moment it was interrupted, or exited by itself at its `-V`
  deadline (watched with `waitid(WNOWAIT)`; a child already reaped by its
  owner counts as exited), minus the video track's length. The log stops
  before the file is finalised, and none of it depends on how long that
  takes. Measured against a window flashing at known host times, changes
  appear in the frames this long after the time the track gives them
  (display latency plus up to one frame): stream recorder 19–68 ms (5 runs),
  system recorder stopping itself at `-V` 36–70 ms (10 runs), GIF −27 to
  +11 ms. For a window recording the pointer is logged relative to
  the window's current origin (the recorders follow the window), a resize
  sets `area_changed`, and the system recorder is given `-o` so its frames
  are the window frame without the shadow. `hide_cursor` sets
  `showsCursor = NO` on the stream recorder (chosen for it in `auto`) and the
  GIF grabs, and implies the track; the system recorder cannot hide the
  pointer (it draws it without `-C`), so the track then says
  `cursor_in_video`.
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
  `zoom` and `smooth_cursor` (`cursor_fx.rs`) use the source's event track:
  clicks inside the recorded area (8 pt margin), except one in the last 0.3 s
  (the click that stopped the recording), become zoom segments (ease in so the view is fully zoomed at the
  click, hold 1.5 s past the last click of a run, ease out; a click before
  the view is back at 1x joins the run and the view pans to it, arriving as
  it happens), the visible rect keeps the frame's aspect and stays inside it,
  and frames are cropped and scaled with a fixed-point bilinear sampler
  (taps computed once per row and column; frames with nothing to apply pass
  through without a copy). Smoothing
  resamples the pointer path at 120 Hz and convolves it with a centred
  Gaussian (σ 0.1 s, so no lag), blending back to the raw path around each
  click; the pointer is drawn as an anti-aliased signed-distance arrow,
  scaled with the zoom. Positions map to pixels by the frame size over the
  area size, so any preset works. Video → video renders through
  `SequentialFrameReader` → effects → `VideoFrameWriter` (each output frame
  shows the source frame on screen at its midpoint) at the requested rate,
  else the recording's (`fps` in the track), else the nominal rate but at
  least 30 and at most 60 (variable-rate stream recordings of a still screen
  store a few frames a second); `original`/`high` encode H.264, `hevc` HEVC.
  Reader and writer are tagged BT.709 so small frames do not shift colour
  from one generation to the next. Rendering is cancelled on the export
  deadline, and `mux_audio` copies the source's audio tracks over the same
  range back in untouched; the GIF pipelines apply the effects to the frames
  they already decode. A plain edit of a recording with a track writes the
  trimmed track next to its output; an effects edit removes any stale track
  of its output's name. A track whose window was resized is refused.
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
  that video, and `subtitles` written later follow a synced video. A video's
  cursor event track goes along under `<synced name>.events.json` (a move
  updates `MediaItem.events`). Tests check
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
The non-blocking overlays (window picker, switchable area selection) need an
event loop that the caller can step out of, so `pick::run_blocking` bootstraps
`NSApplication` once (`app_bootstrap`: accessory policy, `finishLaunching`)
and pumps `run_loop_once` in 50 ms slices until the session answers. That is
what `pick_window` targets (`bugle capture pick-window`) and, with
`[screenshots] window_picker`, the Space toggle of every interactive selection
(`Engine::select_area_or_window`: `bugle capture`, `bugle scroll`, `bugle
record start`) use; off the main thread it refuses with `NoUiHost`.

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
the worker with its reason (a typed `BugleError::Busy`, HTTP 409, when retrying
can help). The picker and the area selection refuse to open over each other,
and one gate (`picker.rs`) is held for a whole pick session and by
`UiHost::select_area`: a second pick is refused rather than queued, and nothing
can open in the gap between two overlays of a session. API picks are also
refused for 3 s after the user dismissed one. Space switches between them: a pick *session* is a small
state machine (`picker::Session`, unit-tested without a screen) that starts in
either mode, flips on each switch and answers with a window or a rect; the
main thread opens the next overlay from the previous one's final callback.
*Capture Area*, *Capture Window* and the three *Record* items run through it;
each picker turn keeps the 30 s timeout and a whole session closes after two
minutes (an area phase never times out mid-drag). A switch leaves Bugle active
for the next overlay; if none follows, the session deactivates Bugle itself. `[screenshots] window_picker = false` restores the immediate
front-most capture (and plain area selection). *Scrolling Capture* runs
through the same session (starting in the area selection); a picked window
scrolls the part of its frame on the display it overlaps most
(`pick::picked_rect`); a pick does not raise the window, so `pick::scroll_rect`
refuses one that another window partly covers. The session itself (`Session`, `window_at`, `begin`)
lives in `bugle_engine::pick`, so the CLI can run it too; `picker.rs` keeps the
app's side (the gate, the cool-down and the worker's `pick`).
`UiHost::pick_window` offers the picker (without the switch) to API callers:
the `pick_window` target kind of `capture` and `record_start` goes through it,
and an engine with neither a host nor in-process overlays (`bugle serve`,
`bugle mcp`) refuses that target up front with
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
Settings → Login Items. With `[general] system_notifications`,
`notifications.rs` also posts to Notification Center from `App::toast` /
`toast_error` and from `on_done` when the quick-access panel takes the
toast's place (direct `toast_show` feedback to panel and editor clicks does
not post). Authorization is requested once at launch from the bundle; the
shim always takes the state from `getNotificationSettings` (a refusal often
arrives as an error, not a plain "no"), and until it is "authorized" (or
when unbundled or switched off) only the in-app feedback shows. While not
authorized, the 1 s tick re-reads the setting every 5 s without prompting.
`--smoke-test` with the switch on only reads the setting and never prompts;
with it off it does not touch the framework; test binaries are never bundles.

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

`--universal` checks up front (`rustc --print target-libdir`) that both
`aarch64-apple-darwin` and `x86_64-apple-darwin` have a standard library,
naming the `rustup target add` command otherwise, then runs one
`cargo build --target` per triple and `lipo -create`s each executable into
`<target dir>/xtask/universal` before assembly. Every xtask build (host or
cross) sets `MACOSX_DEPLOYMENT_TARGET` to the shim's minimum, so both modes
produce the same `LC_BUILD_VERSION` and consecutive host runs still reuse
`target/release`. Verification checks `lipo -archs` of both executables (a
host build only prints them) and runs `--version` on every slice: the
native one directly, x86_64 on Apple silicon through `arch -x86_64` when
Rosetta is installed, otherwise it reports the slice as not executed.
`--dmg` / `cargo xtask dmg` `ditto`s the staged (notarized) bundle next to an
`Applications → /Applications` symlink, `hdiutil create -format UDZO`s it in
the staging directory, signs (Developer ID) and notarizes/staples it when the
bundle was, runs `hdiutil verify`, attaches it read-only with `-nobrowse` at a
mount point under the target directory (force-detaching anything an
interrupted run left there; a guard detaches on any error), checks the link,
the bundle's signature and architectures, and detaches. `release_steps`
fixes the order: all of that happens before `dist/Bugle.app` is replaced, and
the new image is renamed over the old one (never deleted first).

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
image pixels, tool, colour, size, menu picks, commands, typed text) into edits
of the shapes, the crop and the canvas choice, with snapshot undo over all
three, and answers with `Effects` saying what to refresh. Shapes and the crop
stay in original-image pixels; `Editor::document` translates the shapes by the
crop for the renderer and adds the canvas, except while the crop tool is
picked, when the whole image is shown with the crop as an overlay.
`Editor::menus` describes the toolbar's style menu (arrow or redaction styles,
for the tool or the selected shape) and canvas menu (padding, background).
`editor_window.rs` is the glue: it renders `Editor::document` with
`Engine::render_annotations` on a per-window preview thread (only the newest
waiting document is rendered, stale results are dropped) and hands the pixels
and `Annotation::source_origin` to the window, so the preview is the
renderer's output. Only the shape being dragged (and the selection or crop
marks) go to the window as overlay primitives; a shape being moved is left out
of the render meanwhile. After an edit that moves something from the overlay
into the image (a shape released or finished, the crop tool left) the overlay
waits for that render, until the next pointer input; while dragging it never
waits. Renders, saves and copies run under `catch_unwind`, and every preview
job answers, so a renderer failure is a toast rather than a frozen picture or
a window stuck busy. Each picture travels with its own size and
`source_origin`, so a click maps onto what is on screen; what can lag by one
render is the edit state Rust applies it to (tool, crop), and image pixels
per point lag by one main-loop turn: the window owns the zoom and reports it
on a later turn, so nothing it does in reply to Rust calls back into Rust.
Zoomed in, a new picture of another size or origin is scrolled so the image
stays put under the pointer, and the zoom is capped so the canvas stays
within 8192 points.
Save runs `Engine::apply_annotations` on a thread and the window closes only
once it succeeded; `shim/editor.m` frees the Rust handler on a later
main-loop turn, so closing from inside the callback is safe. `--smoke-test`
drives it through `editor_inject` and `editor_zoom`.

## Feature ideas

Bugle is a standalone app; this is the backlog, roughly in the order it would
earn its keep.

* **Video editor extras** — an editing window on top of the headless `edit`
  pipeline (trim, zoom and pointer options with a preview).
* **Audio mixing extras** — per-source levels and ducking work on the mixed
  track; still open: levels for the two-track (unmixed) output, and setting
  them from the menu-bar app instead of the config file.
* **Markup editor extras** — opening a second editor (the window shim keeps
  one editor today). Also: keep the quick-access panel's thumbnails stacked
  when several captures arrive in a row.
* **Packaging extras** — the final bundle id; notification thumbnails
  (an attachment needs a copy of the file, since the notification center
  moves what it is given).
