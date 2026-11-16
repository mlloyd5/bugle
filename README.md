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
| Markup editor | `bugle-app` window: toolbar and zoomable canvas in AppKit, editing logic in Rust, crop, canvas, arrow and redaction styles, multi-line labels; preview (on its own thread) and saved file drawn by the annotation renderer | ✅ |
| Pin to screen | Floating AppKit panel (drag, scroll-to-resize, Esc/double-click closes) | ✅ |
| GIF recording | In-process frame grab loop + `image` GIF encoder | ✅ |
| Video recording | `screencapture -v` (H.264 .mov → remuxed .mp4), microphone, click highlight | ✅ |
| System audio in recordings | ScreenCaptureKit `SCStream` audio tap + `AVAssetWriter` (H.264 + AAC .mp4), microphone as a second track or mixed into one (`--mix-audio`) | ✅ (macOS 14+) |
| Describe image / video frames with a vision model | OpenAI-compatible, Ollama, Anthropic, command | ✅ |
| Transcribe / subtitles (SRT, VTT) | whisper.cpp server, OpenAI-compatible, command | ✅ |
| History | JSON-lines file, ids, delete | ✅ |
| HTTP API + OpenAPI | axum, bearer token | ✅ |
| MCP server (stdio) | JSON-RPC 2.0, image content | ✅ |
| Menu-bar app, global hotkeys | `bugle-app`: `NSStatusItem` via `tray-icon`, Carbon hotkeys via `global-hotkey`, in-app overlays | ✅ |
| Sync to your own storage | folder copy/move, or SigV4 uploads (multipart for large files) to any S3-compatible bucket; a destination per media kind; subtitles follow their video | ✅ |
| Trim and export recordings (presets: trim, H.264/HEVC, 1080p/720p/540p, frame rate, GIF) | `AVAssetExportSession` + video composition, `AVAssetImageGenerator` → GIF, `AVAssetWriter` for GIF → MP4 | ✅ (headless: CLI/API) |
| `Bugle.app` bundle | `cargo xtask bundle` (hardened runtime, ad-hoc or Developer ID) | ✅ ad-hoc; Developer ID signing and notarization not yet exercised |
| Open at Login | `SMAppService.mainAppService` | built, status read verified; registering not yet exercised |

## Build

```bash
cargo build --release
```

The binaries are `target/release/bugle` (CLI + servers) and `target/release/bugle-app`
(menu-bar app). The platform crate compiles a small
Objective-C shim (`crates/bugle-platform/shim/shim.m`) with the Xcode command
line tools; no Xcode project is needed.

### App bundle

```bash
cargo xtask bundle                 # → dist/Bugle.app, ad-hoc signed
cargo xtask bundle --zip           # also dist/Bugle-<version>.zip
cargo xtask verify --smoke-test    # re-check a bundle and launch it briefly
```

`bundle` builds the release binaries (taking their paths from cargo, so a custom
target directory works) and assembles the bundle in `dist/.staging`: an
`Info.plist` with the workspace version, `LSUIElement` (menu-bar only, no Dock
icon) and the microphone usage string; `Bugle.icns`, rendered from the menu-bar
glyph; `Contents/MacOS/bugle-app` and the `bugle` CLI next to it. Both
executables are signed with the hardened runtime and
[`packaging/Bugle.entitlements`](packaging/Bugle.entitlements) (microphone only;
the app is not sandboxed), then checked with `plutil -lint` (Info.plist and the
entitlements embedded in each signature), `codesign --verify --deep --strict`,
`iconutil` and `--version`. Only a bundle that passes replaces `dist/Bugle.app`.

The binaries are built for the host architecture only (arm64 on Apple silicon,
x86_64 on Intel); the bundle does not launch on the other kind of Mac.

When Bugle.app is launched from Finder, the Dock, `open` or as a login item,
the Screen Recording, Microphone and Accessibility prompts and System Settings
name **Bugle** rather than the terminal (running
`Bugle.app/Contents/MacOS/bugle-app` from a shell still attributes permissions
to the terminal). The menu also gains **Open at Login**. Register it from the
installed copy (for example in `/Applications`), not from `dist/`: the login
item points at the bundle's location, so moving the bundle or rebuilding it in
`dist/` leaves a stale entry; turn it off first, or remove the entry in System
Settings → General → Login Items.

To use the CLI from the bundle, link it into a directory on your `PATH` (no
`sudo`; add `~/.local/bin` to `PATH` in your shell profile if it is not there):

```bash
mkdir -p ~/.local/bin
ln -sf /Applications/Bugle.app/Contents/MacOS/bugle ~/.local/bin/bugle
```

| Option | Environment | Default |
|---|---|---|
| `--sign IDENTITY` | `BUGLE_SIGN_IDENTITY` | `-` (ad-hoc) |
| `--bundle-id ID` | `BUGLE_BUNDLE_ID` | `bundle_id` under `[workspace.metadata.bugle]` in the root `Cargo.toml` (the CLI is signed as `<id>.cli`) |
| `--build-number N` | `BUGLE_BUILD_NUMBER` | the workspace version |
| `--out DIR` | — | `dist/` |

The default bundle id is provisional until the project's final domain is
chosen; it is defined only in `Cargo.toml`. macOS keys permission grants and the
login item to the bundle id, so changing it after installing a build resets
those: grant the permissions again and re-enable Open at Login.

An ad-hoc signature is enough to run the bundle on the machine that built it.
macOS identifies an ad-hoc signed app by the hash of its code, so after a
rebuild the old grant may still be listed (even switched on) in System Settings
while capture is denied. Reset the entries and grant them again:

```bash
tccutil reset ScreenCapture <bundle id>
tccutil reset Microphone <bundle id>
tccutil reset Accessibility <bundle id>
```

To distribute the bundle, sign it with a Developer ID Application identity from
your keychain (`security find-identity -v -p codesigning` lists them) and
notarize it:

```bash
# once: store notary credentials in the keychain (prompts for the password)
xcrun notarytool store-credentials bugle-notary --apple-id you@example.com --team-id TEAMID
BUGLE_SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
BUGLE_NOTARY_PROFILE=bugle-notary cargo xtask bundle --notarize
```

`--notarize` zips the staged bundle under a temporary name, submits it with
`xcrun notarytool submit --wait`, staples the ticket, checks it with
`spctl --assess` and only then writes `dist/Bugle-<version>.zip`. Instead of a
keychain profile it accepts an App Store Connect API key: `BUGLE_NOTARY_KEY`
(path to the `.p8` file), `BUGLE_NOTARY_KEY_ID` and `BUGLE_NOTARY_ISSUER`. No
secret is passed on the command line, and identifiers are masked in the log.
`--notarize` fails before building when the identity is ad-hoc or the
credentials are missing or incomplete; any failing step fails the run.

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
bugle record start screen --system-audio --mic --mix-audio --clicks # one audio track, clicks shown

bugle ocr latest                   # Apple Vision, reading order, --json for boxes
bugle ocr latest --engine llama    # OCR through a vision model instead
bugle describe latest --with-ocr --provider ollama
bugle describe <video-id>          # samples 4 frames and describes the recording
bugle transcribe latest
bugle subtitles latest -f vtt --provider whisper

bugle edit latest --start 2.5 --end -1        # trim, audio kept (see note below)
bugle edit latest -s 0:05 -e 0:12 -p 720p -o ~/Desktop/clip.mp4
bugle edit latest -p high --fps 15            # re-encode at 15 fps
bugle edit latest -p gif-small                # recording → chat-sized GIF
bugle edit demo.gif -p hevc                   # GIF → HEVC .mp4
bugle edit --list-presets

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

Trimming and privacy: `bugle edit` (preset `original`) copies the video without
re-encoding only when the cut starts on a keyframe; otherwise it re-encodes, so
nothing you trimmed off stays in the file. `-p lossless` always copies (faster,
no quality loss) but keeps up to one keyframe interval of footage before the
cut inside the file, hidden from players and recoverable with other tools; do
not use it to remove something sensitive.
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
`spotlight`, `image`. A `text` label breaks into lines at `\n` (also `\r\n`, a
lone `\r` and U+2028). Background presets: sunset, ocean, forest, candy, mono, midnight,
peach, mint, any `#hex`, or `gradient:#a,#b[,angle]`.

## Menu-bar app

```bash
cargo run -p bugle-app            # or target/release/bugle-app [--serve] [--config FILE]
```

`bugle-app` is an accessory process (no Dock icon) that puts a camera glyph in the
menu bar and listens for global hotkeys. Everything it does goes through the same
`Engine` as the CLI, so captures land in the configured `save_dir`, appear in
history, and play the shutter sound when `general.shutter_sound` is on.

Menu: Capture Area · Capture Window · Capture Screen ·
Scrolling Capture · Record Video · Record GIF · Stop Recording · Annotate Latest
(opens the markup editor) · OCR Latest (copies the text) · Pin Latest · Recent Captures (last five, click to open) ·
Open History Folder · Start/Stop Agent API · Preferences… (opens the config
file) · Open at Login (only when running from `Bugle.app`) · Quit. While recording, the status item shows `● m:ss`. Other actions
report through a small toast in the top-right corner (thumbnail and file name;
click to open).

After a capture or recording, the **quick-access panel** takes the toast's
place, in the bottom-right corner by default: the thumbnail (click to open)
with **Copy**, **Annotate**, **Pin**, **Sync** and **Reveal** in Finder, plus a
close (×) button. **Annotate** opens the [markup editor](#markup-editor); set
`annotate_app` to a bundle id such as `com.apple.Preview` to open that app
instead, where edits change the saved capture in place. **Sync**
appears when a `[sync]` destination is set and the file is not there yet — so
it is also the retry when the automatic sync failed. The panel never takes
keyboard focus, so it has no keyboard shortcut: it goes away after any button,
the × button, or `timeout_seconds`. It does not time out while the pointer is
over it, and stays up to 2 s after the pointer leaves. Starting another capture
or recording from the menu or a hotkey closes it first so it is not captured.
Configure it in `[quick_access]`:

```toml
[quick_access]
enabled = true            # false: plain toast instead
timeout_seconds = 8.0     # 0: stays until a button or × is clicked
position = "bottom-right" # bottom-left | top-right | top-left
annotate_app = "builtin"  # or a bundle id, e.g. "com.apple.Preview"
```

*Capture Window* dims the screen slightly and highlights the window under the
pointer with its app, title and size; click to capture it. Esc, a right click
or switching to another app cancels, and the picker closes itself after 30
seconds. Esc only reaches the picker while Bugle is the active app; if it ever
seems ignored, a right click always works. Only ordinary windows are pickable:
invisible windows are skipped, and floating panels or always-on-top windows
above the normal window layer are neither picked nor block the window under
them. Set
`[screenshots] window_picker = false` to capture the front-most window at once
instead, as `bugle capture window` and the API do.

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
| `stop_recording`, `sync_latest`, `open_folder`, `toggle_api`, `preferences`, `quit` | — | unbound until set |

Every menu item except *Recent Captures* can be bound; the keys are the names
above. An empty string unbinds an action. Unknown keys, values that are not
strings (for any key, including the defaults above, which are then left
unbound), bindings that do not parse, a combination already used by another
item, and bindings macOS refuses are skipped with a warning in the log and one
toast at launch; the config still loads. ⌘⇧3/4/5 are also the system screenshot
shortcuts; disable those in System Settings → Keyboard → Keyboard Shortcuts →
Screenshots (or pick other keys) so they do not fire twice. The agent API started
from the menu uses `[api]` from the config and serves the in-app overlay for
`select_area`, `pin` and interactive captures — no helper processes.

Screen Recording permission is requested on first launch (granted to the
terminal or app that launched `bugle-app` when run from a shell, or to Bugle
itself when run from [the app bundle](#app-bundle)).

### Markup editor

*Annotate* on the quick-access panel and *Annotate Latest* in the menu (bind it
with `annotate_latest` in `[hotkeys]`) open the newest screenshot in a window:
a toolbar above the image, which is scaled to fit until you zoom.

| Tool | Key | Draws |
|---|---|---|
| Select | V | click a shape to select it, drag to move, drag a handle to resize, Delete to remove; double-click a label to retype it |
| Arrow, Line | A, L | drag; Shift snaps to 45° steps |
| Rectangle, Ellipse | R, O | drag; Shift makes a square or circle |
| Pen | P | freehand stroke |
| Text | T | click, type, Return (⇧Return or ⌥Return starts a new line; Esc cancels) |
| Highlight | H | marker-pen band in the current colour |
| Numbered step | N | click to place 1, 2, 3… |
| Redact | B | hides the area: pixelate, blur, black out or secure blur |
| Spotlight | S | dims everything outside the area |
| Crop | C | drag the part to keep; drag inside it to move it, a corner to resize it; Delete removes the crop |

The swatches and S/M/L set the colour and the stroke or text size for new
shapes, and restyle the selected one. The **style** menu does the same for
arrows (standard, double-headed, slim, tapered) and redactions. The
**canvas** menu puts padding (small, medium, large) and a background
(transparent, white or a gradient preset) around the image; picking a
background turns medium padding on. While the crop tool is picked the whole
image shows with the cropped-away part dimmed; the crop and canvas show as
soon as you pick another tool, and shapes keep their place in the original
image, so the crop can change later. Strokes and text are sized relative to
the image, so they look the same on a Retina capture as on a small one. ⌘Z and
⇧⌘Z undo and redo (crop and canvas changes too); Esc backs out of a drag,
then the selection, then the tool, and never closes the window. ⌘+ and ⌘−
zoom in and out (10% to 800%), ⌘1 shows actual pixels and ⌘0 fits the
window again; pinching, a two-finger double tap and the zoom menu in the
corner do the same, and a zoomed image scrolls.

**Save** (⌘S) writes an annotated copy as a new history item (kind
`annotated`, named and synced like any other capture) and leaves the original
alone; the quick-access panel then offers the copy. **Copy** (⌘C) puts the
annotated image on the clipboard and keeps the window open. Closing with
unsaved shapes asks first. One editor is open at a time.

What you see is what is saved: finished shapes are drawn by the same renderer
as `bugle annotate`, and a saved document can use everything that command
can. Only the shape under the pointer is drawn by the window, so an arrow
head or a redaction can look slightly different while you drag it. The
preview renders on a thread of its own, so a very large capture never makes
the window stall; while it renders, the old picture stays up.

The editor's window, shapes, selection, menus, crop, canvas, zoom and undo are
checked by `bugle-app --smoke-test`, which feeds it synthetic events rather
than clicks (keeping window captures in the temp directory when
`BUGLE_SMOKE_KEEP` is set, and saving a copy too when `BUGLE_SMOKE_SAVE` is
set).

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
`[api]` (bind address, token, CORS), `[hotkeys]` and `[quick_access]` for the
menu-bar app and `[sync]` below.

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

Both transports expose the same 24 tools (`capture`, `list_windows`, `ocr`,
`describe`, `annotate`, `scroll_capture`, `record_start/stop/status`, `transcribe`,
`subtitles`, `edit_video`, `history`, `pin`, `sync`, …). `capture` returns the PNG inline so an agent can
look at what it took.

## Architecture

Eight crates plus an `xtask` for packaging; see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the details.

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
xtask           `cargo xtask bundle`: assemble, sign and verify Bugle.app
```

## Why "Bugle"?

The Daily Bugle is the paper that *always* needs another picture of Spider-Man.
`jameson` (the editor who demands them) serves the HTTP API; `parker` (the one who
actually takes them) is the MCP server.
