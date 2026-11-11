# 002 — Recording lifecycle: lost recordings, stuck state, unbounded memory

Status: open
Severity: high
Area: `crates/bugle-engine/src/record.rs`

## Problems

1. **Interactive start can overwrite an active recording.** `record_start` (record.rs:96-100) drops the state lock while
   the user drags a selection, re-takes it, and never re-checks `g.is_some()`. A second `record_start` that got in
   meanwhile is overwritten at line 207: its `screencapture` child is orphaned and keeps recording, or its GIF thread
   runs forever.
2. **`max_seconds` never finalises.** After the deadline the GIF thread / `screencapture -V` / the stream stop by
   themselves, but the state stays `Active` (elapsed keeps counting) until someone calls `record_stop`. Over the API an
   agent that passes `max_seconds` and polls `record_status` never sees it end. `RecordingState::Finishing` exists and
   is never used.
3. **Nothing stops the recorder when the state is dropped.** No `Drop` for `Active`: if the engine goes away (process
   exit paths, test teardown) the `screencapture` child is left running and the GIF thread is detached.
4. **GIF frames are held in memory without a bound** (record.rs:328). A forgotten GIF recording at 12 fps and 1200 px
   grows by tens of MB per second until the process dies. Resize can also produce a 0-pixel dimension for extreme
   aspect ratios (record.rs:355-356 truncates).
5. `screencapture -R` gets `f64` Display output (`10.5,20,300.25,200`); it wants integers.
6. `AllScreens` silently records only the main display (record.rs:102).
7. Target rects are not validated (empty, negative, NaN, infinite) before a backend is started.
8. When `record_stop` fails after the backend stopped, `*.rec.mov` / `*.rec.mp4` temp files are left behind.
9. `screencapture` failing immediately (bad window id, permission) is only noticed at stop time.

## Wanted

- Re-check for an active recording after re-acquiring the lock; return the "already in progress" error.
- `record_status` reports `Finishing` once the backend has stopped by itself (child exited, GIF thread finished,
  stream auto-stopped) with `elapsed_seconds` frozen at that point. A `record_start` that finds a finished-but-unreaped
  recording finalises it first (same path as `record_stop`) instead of failing.
- `Drop` for the active recording: SIGINT + wait for the child, stop flag + join for the GIF thread.
- A hard cap for GIF recordings (frame count and/or total decoded bytes; pick a sensible default, e.g. stop at about
  1 GiB of frames or 10 minutes, whichever comes first) that ends the loop and logs why. Clamp resized dimensions to >= 1.
- Round the `-R` rect to integers.
- `AllScreens` for recordings: return `BugleError::Invalid` saying which targets are supported, rather than silently
  recording one display.
- Validate rects (`is_finite`, `w,h >= 2`) with `BugleError::Invalid`.
- Clean up temp files on the error paths of `record_stop`.
- After spawning `screencapture`, wait briefly (about 300 ms) and fail `record_start` with its stderr if it already exited.
- Unit tests for whatever can run without Screen Recording permission (state machine pieces, rect validation, rect
  formatting). Factor small pure helpers if that makes them testable.

## Out of scope

The Objective-C recorder in `shim.m` (issue 006), `scroll.rs`, `lib.rs`.
