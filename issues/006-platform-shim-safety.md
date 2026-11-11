# 006 — Platform shim: data race on the last-error global, unchecked sizes

Status: open
Severity: high
Area: `crates/bugle-platform/shim/{shim.m,shim.h}`, `crates/bugle-platform/src/{lib.rs,ffi.rs}`

## Problems

1. **`g_last_error` is an ARC-managed global written from many threads** (shim.m:20-27, and every `g_last_error = nil`
   at the top of the capture/OCR/export functions). The HTTP server runs engine calls on the blocking pool, so two
   concurrent requests race on a strong pointer: over-release and a crash, or one request reporting the other's error.
2. `cg_from_rgba`, `bugle_ocr`, `bugle_register_translation` trust `w`/`h` and the buffer length; the Rust side passes
   `width() as i32`. Nothing checks that the image fits in `i32` or that `w * h * 4` matches the slice.
3. `bugle_capture_area` builds `SCStreamConfiguration` width/height from caller-supplied doubles with no upper bound
   and no NaN check; `llround(NaN)` is undefined.
4. `windows()` (lib.rs:114-119) creates a `Vec` with capacity 1024 and `set_len`s it after C wrote into uninitialised
   memory. It works, but it is easy to get wrong; and more than 1024 windows are silently truncated.
5. `copy_str` can cut a multi-byte UTF-8 sequence in half at the buffer limit; Rust then lossily decodes it.
6. `bugle_ocr`: `strdup(top.string.UTF8String)` with a NULL `UTF8String` crashes; `calloc` result is unchecked.
7. The stream recorder's `failure` property is written on the capture queues and read in `finish` without the lock.
   `startWithFilter:` failing after `startCapture` succeeded (not possible today, but the mic path is close) would
   leave the stream running.
8. A screenshot/enumeration that times out leaves its completion block to fire later; if it then delivers an image,
   that `CGImageRetain` is never balanced.
9. `bugle_accessibility_trusted`, the clipboard and `NSSound`/`NSWorkspace` calls are made from arbitrary threads.
   AppKit pasteboard access off the main thread is tolerated; note it in a comment or hop to main where cheap.

## Wanted

- Per-thread last error (a `_Thread_local char *` that `set_error` replaces and `bugle_last_error` hands over, or a
  lock around a copied string keyed by thread). No ARC object in a plain global that several threads assign.
- Dimension and length checks on both sides of the FFI; `BugleError::Invalid` on the Rust side for oversized images.
- Reject non-finite rects and cap pixel sizes (for example 16384 per side) in the capture entry points.
- `windows()`: zero-initialised buffer, or ask for the count first; no silent truncation.
- UTF-8-safe truncation in `copy_str`.
- NULL checks in `bugle_ocr`.
- Guard `failure` with the existing lock.
- Balance the late image in the timeout path (for example a shared flag the block checks before retaining).
- Keep `cargo build` warning-free for the shim and keep the two existing platform tests passing. Add Rust tests for the
  new validation.

## Notes

Screen Recording is granted on the development Mac, Accessibility is not. `@available` cannot be used (see the comment
at shim.m:162). Do not raise the deployment target.

## Out of scope

Behavioural changes to capture, OCR or recording beyond the safety fixes above.
