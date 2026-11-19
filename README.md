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
| System audio in recordings | ScreenCaptureKit `SCStream` audio tap + `AVAssetWriter` (H.264 + AAC .mp4), microphone as a second track or mixed into one (`--mix-audio`) with per-source levels (on either output), optional ducking of system audio under the microphone and a peak limiter, live level meter while recording | ✅ (macOS 14+) |
| Describe image / video frames with a vision model | OpenAI-compatible, Ollama, Anthropic, command | ✅ |
| Transcribe / subtitles (SRT, VTT) | whisper.cpp server, OpenAI-compatible, command | ✅ |
| History | JSON-lines file, ids, delete, `history prune` for deleted files | ✅ |
| HTTP API + OpenAPI | axum, bearer token | ✅ |
| MCP server (stdio) | JSON-RPC 2.0, image content | ✅ |
| Menu-bar app, global hotkeys | `bugle-app`: `NSStatusItem` via `tray-icon`, Carbon hotkeys via `global-hotkey`, in-app overlays | ✅ |
| Sync to your own storage | folder copy/move, or SigV4 uploads (multipart for large files) to any S3-compatible bucket; a destination per media kind; subtitles follow their video | ✅ |
| Trim and export recordings (presets: trim, several cuts in one export, H.264/HEVC, 1080p/720p/540p, frame rate, GIF) | `AVAssetExportSession` + video composition, `AVAssetImageGenerator` → GIF, `AVAssetWriter` for GIF → MP4 | ✅ (CLI/API, and the video editor window) |
| Auto-zoom on clicks (a level for all, or per click), smoothed pointer | pointer/click event track recorded next to the video (`clip.mp4.events.json`); frames re-rendered in Rust (`AVAssetReader` → `AVAssetWriter`, audio copied back in) | ✅ (CLI/API, and the video editor window); track timing measured on real recordings, zoom checked with hand-written clicks, live clicks not exercised |
| Video editor | `bugle-app` window for videos and GIFs: preview, timeline with audio waveform, click markers, in/out handles and cuts, playback, zoom (per click too) and pointer options, preset, export through the same edit pipeline | ✅ driven by synthetic events in `--smoke-test`, also as in-process mouse and key events through AppKit (see Video editor); timeline waveform checked on a synthetic tone (levels handed to it, and drawn in a window capture); a person's hardware mouse and keyboard not exercised |
| `Bugle.app` bundle | `cargo xtask bundle` (hardened runtime, ad-hoc or Developer ID) | ✅ ad-hoc; Developer ID signing and notarization not yet exercised |
| Disk image, universal binary | `cargo xtask dmg` (`hdiutil`, `/Applications` link), `--universal` (`lipo`) | ✅ DMG; universal needs both Rust targets installed |
| System notifications | `UNUserNotificationCenter` from Bugle.app, alongside the toast (`[general] system_notifications`), with the capture's thumbnail attached | built; reading the authorization verified from the bundle, posting and the attached thumbnail unverified |
| Open at Login | `SMAppService.mainAppService` | built, status read verified; registering not yet exercised |

## Build

```bash
cargo build --release
```

The binaries are `target/release/bugle` (CLI + servers) and `target/release/bugle-app`
(menu-bar app). The platform crate compiles a small
Objective-C shim (`crates/bugle-platform/shim/shim.m`) with the Xcode command
line tools; no Xcode project is needed.

`cargo xtask check` runs `cargo clippy --workspace --all-targets -- -D warnings` and
`cargo test --workspace` and exits non-zero if either fails; run it before sending a change.

### App bundle

```bash
cargo xtask bundle                 # → dist/Bugle.app, ad-hoc signed
cargo xtask bundle --zip           # also dist/Bugle-<version>.zip
cargo xtask dmg                    # also dist/Bugle-<version>.dmg (= bundle --dmg)
cargo xtask bundle --universal     # arm64 + x86_64 in one bundle
cargo xtask verify --smoke-test    # re-check a bundle, then run its smoke test (~30 s)
```

`bundle` builds the release binaries (taking their paths from cargo, so a custom
target directory works) and assembles the bundle in `dist/.staging`: an
`Info.plist` with the workspace version, `LSUIElement` (menu-bar only, no Dock
icon) and the microphone usage string; `Bugle.icns`, scaled from the artwork in
[`packaging/Bugle-icon-1024.png`](packaging/Bugle-icon-1024.png); `Contents/MacOS/bugle-app` and the `bugle` CLI next to it. Both
executables are signed with the hardened runtime and
[`packaging/Bugle.entitlements`](packaging/Bugle.entitlements) (microphone only;
the app is not sandboxed), then checked with `plutil -lint` (Info.plist and the
entitlements embedded in each signature), `codesign --verify --deep --strict`,
`iconutil` and `--version`. Only a bundle that passes replaces `dist/Bugle.app`.

`--smoke-test` (on `verify` and `bundle`) then runs `bugle-app --smoke-test`
from the bundle with a throwaway config in a temp directory: saves, history and
`TMPDIR` go there, sync is off and no global hotkey is registered, so your own
config, captures and sync destination are never read or touched; the directory
is removed afterwards (kept with `BUGLE_SMOKE_KEEP=1`). The app's opt-ins
pass through from your environment: `BUGLE_SMOKE_CLIPBOARD=1` pastes through
the real clipboard, `BUGLE_SMOKE_MICROPHONE=1` records the microphone and
`BUGLE_SMOKE_SAVE=1` saves and exports (into the throwaway folder). From the
release bundle the run took about 32 s; it is killed after 180 s
(`BUGLE_SMOKE_TIMEOUT=<seconds>` changes that, and a bad value fails before
anything is built).

By default the binaries are built for the host architecture only (arm64 on
Apple silicon, x86_64 on Intel), and the bundle does not launch on the other
kind of Mac. `--universal` builds both (`--target aarch64-apple-darwin` and
`--target x86_64-apple-darwin`) and merges each executable with
`lipo -create`; verification then requires `lipo -archs` to report
`arm64 x86_64` for both, and runs `--version` on each slice the Mac can
execute (the x86_64 slice through Rosetta on Apple silicon, when installed;
otherwise it says the slice was not executed). Every xtask build sets
`MACOSX_DEPLOYMENT_TARGET=13.0`, the shim's minimum. It needs the Rust standard
library for both targets and stops before building, naming the command, when
one is missing:

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
```

(Homebrew's `rust` formula has no rustup and cannot add targets; use a rustup
toolchain for universal builds.)

`--dmg` (or `cargo xtask dmg`, which takes the same options) writes
`Bugle-<version>.dmg` from the finished staged bundle: a compressed (UDZO),
read-only HFS+ image named "Bugle <version>" holding `Bugle.app` and an
`Applications` link for drag-to-install. With a Developer ID identity the image
is signed as well, and with `--notarize` it is notarized and stapled too. It is
checked with `hdiutil verify`, then mounted read-only (out of sight, under the
target directory) to check the link, `codesign --verify --deep --strict` the
bundle on it and its architectures. Only after all of that are `dist/Bugle.app`
and then the previous image replaced, so a failing image leaves both as they
were.

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
| `--universal` | — | off (host architecture) |
| `--dmg` | — | off (`cargo xtask dmg` turns it on) |
| `--zip` | — | off |

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
bugle capture                      # drag an area (TTY; Space: pick a window) / main screen (non-TTY)
bugle capture pick-window          # click a window in the window picker
bugle capture screen --display 3
bugle capture window --app Safari --no-shadow -c       # -c copies to clipboard
bugle capture area --rect 100,100,800,600 -f jpeg -q 85 -o ~/Desktop/x.jpg
bugle capture window --title "Lab power" --ocr --describe --pin
bugle capture all                  # every display, stitched

bugle scroll                       # drag the scrolling area (Space: pick a window), then keep the mouse over it
bugle scroll --rect 0,120,1200,800 --max-frames 60 --step -10

bugle record start area --rect 0,30,1280,720 -f gif --fps 10 -s 8
bugle record start window --app Xcode -f mp4 --mic --subtitles   # Ctrl-C to stop
bugle record start screen -f mp4 --system-audio -s 30              # ScreenCaptureKit backend
bugle record start screen --system-audio --mic --mix-audio --clicks # one audio track, clicks shown
bugle record start screen --system-audio --mic --mix-audio --mic-gain 1.5 --duck  # voice over, system audio ducked
bugle record start screen --cursor-events -s 30   # also writes <file>.events.json (pointer + clicks)
bugle record start screen --hide-cursor -s 30     # pointer left out of the frames, logged instead

bugle ocr latest                   # Apple Vision, reading order, --json for boxes
bugle ocr latest --engine llama    # OCR through a vision model instead
bugle describe latest --with-ocr --provider ollama
bugle describe <video-id>          # samples 4 frames and describes the recording
bugle transcribe latest
bugle subtitles latest -f vtt --provider whisper

bugle edit latest --start 2.5 --end -1        # trim, audio kept (see note below)
bugle edit latest -s 0:05 -e 0:12 -p 720p -o ~/Desktop/clip.mp4
bugle edit latest --cut 3..5 --cut 1:02..1:10 # remove two ranges, join the rest
bugle edit latest -p high --fps 15            # re-encode at 15 fps
bugle edit latest -p gif-small                # recording → chat-sized GIF
bugle edit demo.gif -p hevc                   # GIF → HEVC .mp4
bugle edit latest --zoom                      # zoom 2x on every click (--zoom=1.5 for less)
bugle edit latest --zoom --click-zoom 4.2=3 --click-zoom 9=1   # the click at 4.2 s 3x, the one at 9 s not at all
bugle edit latest --smooth-cursor --zoom      # --hide-cursor recordings: smoothed pointer drawn in
bugle edit --list-presets

bugle annotate latest --arrow 200,300,500,150 --box 100,100,400,200 \
    --text "120,60:Look here" --counter 150,400 --redact 600,500,300,100 \
    --canvas sunset --padding 60 --open
bugle annotate latest --spec annotations.json      # full document, see below
bugle annotate --schema                            # example of every shape

bugle pin latest --at 1900,120,600,300 --opacity 0.95
bugle windows | bugle displays | bugle history -n 10 | bugle delete <id> --file
bugle history prune [--dry-run]    # drop entries whose file was deleted (files untouched)
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
not use it to remove something sensitive. With `--cut` (repeatable,
`START..END` in the same time forms) the same holds for every kept part:
`original` copies only when each part starts on a keyframe.

Zoom and pointer smoothing: a recording made with `--cursor-events` (or
`[recordings] cursor_events = true`) gets an event track next to it,
`<file>.events.json` (`clip.mp4` → `clip.mp4.events.json`): the pointer
position and the mouse buttons, polled 60 times a second (a press shorter than
one poll, about 16 ms, can be missed), in points relative to the recorded area,
on the video's timeline (placed on it to within about one frame; with
`backend = "auto"` such recordings use the ScreenCaptureKit recorder, whose
clock the track shares). `bugle edit
--zoom` (2x) or `--zoom=FACTOR` uses it to ease in on each click inside the
recorded area (fully zoomed as it happens, held 1.5 s after the last of a run of
clicks, then eased out; nearby clicks pan from one to the next; the click that
stops the recording is ignored). `--click-zoom TIME=FACTOR` (repeatable) sets
the zoom of one click, named by its time in the recording (within 0.05 s; an
error lists the clicks there are), overriding `--zoom` for it: `1` leaves that
click out, and without `--zoom` only the named clicks zoom. Within a run of
clicks the view changes level along with the pan. `--hide-cursor` records without the pointer
(ScreenCaptureKit recorder) and implies the track; `--smooth-cursor` then draws
the pointer back along a smoothed path that still lands exactly on each click.
These edits re-render every frame (at the recording's frame rate, at least
30 fps) and copy the audio tracks back in untouched; `original` and `high` then
encode H.264 at the source size (about 0.1 bit per pixel per frame), `hevc`
HEVC, so `original` does not keep a HEVC source's codec. A plain trim of such a
recording writes the trimmed track next to its output, so you can cut first and
zoom later. The track moves, syncs and is deleted with its video. Window
recordings: the pointer is logged relative to the window as it moves; a window
resized during the recording cannot be zoomed or smoothed, and the system
recorder leaves out the window shadow when it logs events, so the frames match
the window. The format is documented in `crates/bugle-core/src/events.rs`.

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
lone `\r`, U+2028 and U+2029). Background presets: sunset, ocean, forest, candy, mono, midnight,
peach, mint, any `#hex`, or `gradient:#a,#b[,angle]`.

## Menu-bar app

```bash
cargo run -p bugle-app            # or target/release/bugle-app [--serve] [--config FILE]
```

`bugle-app` is an accessory process (no Dock icon) that puts a horn glyph in the
menu bar and listens for global hotkeys. Everything it does goes through the same
`Engine` as the CLI, so captures land in the configured `save_dir`, appear in
history, and play the shutter sound when `general.shutter_sound` is on.

Menu: Capture Area · Capture Window · Capture Screen ·
Scrolling Capture · Record Video · Record GIF · Record Window · Stop Recording · Audio Levels
(microphone and system-audio levels, ducking and its amount for the next recordings, each a preset
or a custom value typed in with *Custom…*, saved to
`[recordings]` in the config file; they apply when `microphone` and `system_audio` are both on
under `[recordings]`, on macOS 14 or later, and the menu says so otherwise) · Annotate Latest
(opens the markup editor) · Edit Latest Recording (opens the [video editor](#video-editor)) · OCR Latest (copies the text) · Pin Latest · Recent Captures (last five, click to open) ·
Open History Folder · Start/Stop Agent API · Preferences… (opens the config
file) · Open at Login (only when running from `Bugle.app`) · Quit. While recording, the status item shows `● m:ss`;
a recording with system audio or the microphone (the ScreenCaptureKit recorder) adds a live level meter: one
bar glyph per source after the time (microphone first) and, under Stop Recording, a line per source with its
peak in dB and a `!` clip mark near full scale. Levels are measured as recorded, before any gain or ducking.
Other actions
report through a small toast in the top-right corner (thumbnail and file name;
click to open).

With `system_notifications = true` under `[general]`, Bugle.app also posts to
Notification Center (`UNUserNotificationCenter`), named and badged as Bugle:
every finished menu or hotkey action (capture, recording, OCR, pin, sync —
whether the quick-access panel or the toast shows it; clicking a saved file's
notification opens it), a markup-editor save that falls back to the toast, and
every error. A notice that has a thumbnail in the toast carries it in the
notification too. Immediate feedback to a click in the panel or the editor (such as
"Copied to clipboard") and the agent API's start message stay in-app only.
macOS asks for permission at the first launch with the switch on. The panel or
toast is shown either way: an unbundled `bugle-app`, a denied permission or
the switch left off (the default) means in-app feedback only. While not
authorized the app re-reads the setting every few seconds without prompting,
so turning Bugle on in System Settings → Notifications applies within seconds.

After a capture or recording, the **quick-access panel** takes the toast's
place, in the bottom-right corner by default: the thumbnail (click to open)
with **Copy**, **Annotate**, **Pin**, **Sync** and **Reveal** in Finder, plus a
close (×) button; a video or GIF recording gets **Edit** instead of Annotate and Pin,
which opens the [video editor](#video-editor). **Annotate** opens the [markup editor](#markup-editor); set
`annotate_app` to a bundle id such as `com.apple.Preview` to open that app
instead, where edits change the saved capture in place. **Sync**
appears when a `[sync]` destination is set and the file is not there yet — so
it is also the retry when the automatic sync failed. The panel never takes
keyboard focus, so it has no keyboard shortcut: it goes away after any button,
the × button, or `timeout_seconds`. It does not time out while the pointer is
over it, and stays up to 2 s after the pointer leaves. Starting another capture
or recording from the menu or a hotkey hides the panels while it runs, so they
are not captured; when it is done they come back with the new capture's panel
in the corner and the earlier ones stacked behind it, each with its own
timeout (hovering any of them holds them all). Timeouts stop while the panels
are hidden. Up to four panels stack; a
fifth capture closes the oldest.
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
`bugle capture pick-window` (and `bugle record start pick-window`) shows the
same picker from a terminal.

Space switches between the window picker and the area selection while either
is up, so *Capture Area* and *Capture Window* are one gesture apart; the hint
at the bottom of the screen says which mode is on. In the area selection Space
only works before you start dragging. *Record Video*, *Record GIF* and *Record
Window* use the same pair of overlays: Record Window starts in the picker and
records the window you click (MP4), the other two start in the area selection.
*Scrolling Capture* starts in the area selection too; a picked window scrolls
the part of it that is on screen. From a terminal, `bugle capture`, `bugle
scroll` and `bugle record start` without a target offer the same Space toggle
in their area selection, which then also closes after two minutes (with
`window_picker = false` it waits as long as you like). Scrolling a picked
window needs it in front: when another window covers part of it, Bugle says
so instead of capturing.
A picked window is recorded by id, the way `record_start` with a window target
records it, not as the screen area under it. The two overlays together close after two minutes
(each picker turn still after 30 seconds; never in the middle of a drag).
With `window_picker = false` the menu items (and the CLI's interactive
selection) do without the picker: Space does nothing, Capture Area, Scrolling
Capture and the Record items use the plain area selection, and
*Record Window* records the front-most window at once. An API caller that asks
for `pick_window` explicitly still gets the picker.

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
| `record_window`, `stop_recording`, `annotate_latest`, `edit_latest`, `sync_latest`, `open_folder`, `toggle_api`, `preferences`, `quit` | — | unbound until set |

Every menu item except *Recent Captures* can be bound; the keys are the names
above. An empty string unbinds an action. Unknown keys, values that are not
strings (for any key, including the defaults above, which are then left
unbound), bindings that do not parse, a combination already used by another
item, and bindings macOS refuses are skipped with a warning in the log and one
toast at launch; the config still loads. ⌘⇧3/4/5 are also the system screenshot
shortcuts; disable those in System Settings → Keyboard → Keyboard Shortcuts →
Screenshots (or pick other keys) so they do not fire twice. The agent API started
from the menu uses `[api]` from the config and serves the in-app overlay for
`select_area`, `pin` and interactive captures — no helper processes. It also
offers the window picker: a `capture` or `record_start` whose target is
`{"kind":"pick_window"}` shows the picker and uses the window the user clicks
(Space does not switch to the area selection there), whatever
`window_picker` says. One overlay request is served at a time: a second pick
while one is open, or one within 3 seconds of the user dismissing the picker,
is refused with HTTP 409 instead of queuing. `bugle serve` and `bugle mcp`
cannot show the picker and answer with an error (HTTP 503) that says so.
The CLI commands run in a terminal show it themselves (`pick-window` target).

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
unsaved shapes asks first. Several editors can be open at once, each in a
window of its own; annotating a capture that already has an editor open
brings that window forward instead.

What you see is what is saved: finished shapes are drawn by the same renderer
as `bugle annotate`, and a saved document can use everything that command
can. Only the shape under the pointer is drawn by the window, so an arrow
head or a redaction can look slightly different while you drag it. The
preview renders on a thread of its own, so drawing on a very large capture
stays responsive (handing the finished picture to the window still copies it
on the main thread); while it renders, the old picture stays up. A preview
that cannot be drawn, or a canvas too large to render, is reported in a
toast.

The editor's window, shapes, selection, menus, crop, canvas, zoom and undo are
checked by `bugle-app --smoke-test`, which feeds it synthetic events rather
than clicks (keeping window captures in the temp directory when
`BUGLE_SMOKE_KEEP` is set, and saving a copy too when `BUGLE_SMOKE_SAVE` is
set). It also draws a rectangle with a synthetic mouse drag on the canvas,
undoes and redoes it with ⌘Z / ⇧⌘Z and types a two-line label (⇧Return,
then Return), with mouse and key events built inside the app and sent to the
window, so the window's own hit-testing and key handling take part; they are
not a person's hardware input (the window server's part is left out).

### Video editor

*Edit* on the quick-access panel after a recording opens that recording;
*Edit Latest Recording* in the menu (bind it with `edit_latest` in
`[hotkeys]`) opens the newest recording in the history. Either way the video
(`.mp4`, `.mov`, `.m4v`) or GIF opens in a window: the frame at the playhead, a
timeline below it and a row of controls.

| Control | Key | Does |
|---|---|---|
| Timeline | click, drag | moves the playhead (a click snaps it onto a click marker it lands next to; a drag scrubs freely); drag the tabs at either end of the highlighted range to move the in and out points. Across the track: the recording's audio waveform (every audio track mixed). Under it: a diamond for each click auto-zoom follows, filled when the click zooms, hollow when it does not, larger for the click at the playhead |
| Play | Space | plays the clip from the playhead (from the in point when the playhead is outside it), jumps over cuts, stops at the out point |
| Set In, Set Out | I, O | start or end the clip at the playhead |
| Cut | C | starts a cut at the playhead (the button becomes *End Cut*); pressed again, removes the part between the two (struck out on the timeline; touching cuts merge) |
| Restore | Delete | restores the cut under the playhead, or drops the cut being marked (so does Esc) |
| | ← → | one frame back or forward |
| | ↑ ↓ (Home, End) | to the in or out point |
| Zoom | | zoom in on clicks, 1.5× to 3× (recordings made with `cursor_events` or `hide_cursor`) |
| Click | | the zoom of the click at the playhead (from its ease-in until its zoom is over): *Default* (what *Zoom* says), *Off* or 1.5× to 3×; so with *Zoom* off only the clicks given a level zoom (`bugle edit --click-zoom`) |
| Smooth pointer | | draw the pointer along a smoothed path (recordings made with `hide_cursor`; on by default for them) |
| Preset | | the export format, the presets of `bugle edit`; *Lossless (copy)* keeps the frames as recorded, so it turns zoom, click zooms and smoothing off (turning any on goes back to *Original*); a GIF has no *Lossless* (its cuts are always exact) |
| Export | ⌘E | writes the clip as a new history item |

The preview shows what the export will: frames are decoded exactly at the
playhead, scaled to the window, and the zoom and pointer are drawn by the same
code as `bugle edit --zoom --smooth-cursor`. Playback stays in real time, so
on a slow machine it skips frames rather than slowing down (at most 30 frames a
second are drawn). **Export** runs the same edit as `bugle edit` with the
chosen range, cuts and options (the audio is kept, cut where the video is),
leaves the recording alone and
shows the quick-access panel for the new file; the window stays open with
your settings if the export fails. A recording without an event track can be
trimmed and exported, with the zoom and pointer options greyed out. One video
editor is open at a time. A GIF opens at once (its timeline is read from the
frame headers); its frames are then decoded once in the background, scaled to
the preview, so seeking is instant. The preview frames are kept within 256 MB:
past that they are scaled down to no less than 640 px on the long edge, and
beyond that only every second (third, ...) frame is shown while scrubbing a
very long GIF. The export always uses every frame. *Original* exports a GIF
again.
Quit waits for a running export: it is refused until the export finishes.
`bugle-app --smoke-test` opens it on a synthetic recording (two tone bursts
as its audio), checks the levels handed to the timeline against the tones
and against a second read of the file, checks a capture of the window shows
the waveform at those heights, and drives it with synthetic events
(exporting with `BUGLE_SMOKE_SAVE`).

A second pass sends the window mouse and key events built inside the app,
so its own hit-testing and key handling take part, and checks the playhead,
in and out points, cuts and choices after each step. It clicks the timeline
to seek (and 3 pt from a click marker, which snaps; a drag ending there does
not), drags the in and out handles, presses Space, I, O, C, Delete, forward
delete, the arrows, Home, End and Esc, clicks Set In, Set Out, Cut, Restore,
play and *Smooth pointer*, and closes with ⌘W, or with ⌘E (which exports)
under `BUGLE_SMOKE_SAVE`; the Export button is not clicked. The preset, zoom
and click-zoom pop-ups are opened with a click, closed again without a
choice, and the item is then chosen through the menu
(`performActionForItemAtIndex:`), not by pointing in the open menu. These
events never pass through the window server, so they are not the same as a
person's mouse and keyboard. The same pass types into a text prompt (⌘A,
Return, Esc; ⌘V only with `BUGLE_SMOKE_CLIPBOARD=1`, since it goes through
the real clipboard). A check that fails or cannot run is printed as
`FAILED`.

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
`[screenshots]`, `[recordings]` (backend, audio, mix levels and ducking), `[ocr]`, `[markup]` defaults,
`[api]` (bind address, token, CORS), `[hotkeys]` and `[quick_access]` for the
menu-bar app and `[sync]` below.

The HTTP API is meant for local agents. Setting a token is recommended; without
one any local process can call it. Binding to a non-loopback address (e.g.
`--bind 0.0.0.0:4848`) is refused unless a token is set, a loopback listener
rejects requests whose `Host` is not `127.0.0.1`, `localhost` or `[::1]`, and
`cors = true` only takes effect together with a token. Without a token, requests
a browser sends from another origin (an `Origin` other than the API's own
address, or `Sec-Fetch-Site` other than `same-origin`/`none`) get 403 on every
route, so a web page cannot trigger actions; `curl`, scripts and agents send
neither header and need nothing extra. Web pages need a token plus `cors = true`.

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

Eight crates plus an `xtask` for packaging and checks; see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the details.

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
xtask           `cargo xtask bundle`: assemble, sign and verify Bugle.app; `cargo xtask check`: clippy + tests
```

## Why "Bugle"?

The Daily Bugle is the paper that *always* needs another picture of Spider-Man.
`jameson` (the editor who demands them) serves the HTTP API; `parker` (the one who
actually takes them) is the MCP server.
