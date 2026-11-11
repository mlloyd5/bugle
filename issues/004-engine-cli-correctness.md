# 004 — Engine and CLI correctness

Status: closed (fixed on `audit/fixes`)
Severity: medium
Area: `crates/bugle-engine/src/{lib.rs,capture.rs}`, `crates/bugle-api/src/actions.rs`, `crates/bugle-cli/src/cli.rs`,
`crates/bugle-core/src/lib.rs` (error enum), `crates/bugle-app/src/main.rs` (cancel detection only)

## Problems

1. **`[ocr] engine = "<provider>"` breaks OCR.** `ocr_image` (lib.rs:165) reads the config default and bails for
   anything but `vision`, while the callers (`actions.rs:289`, `cli.rs:576`) only route to the AI path when the
   *request* names an engine. Setting a provider as the default OCR engine makes every `ocr` call fail.
2. **`output` extension and format can disagree.** `capture --output shot.jpg` with the default format writes PNG bytes
   into `shot.jpg` (`output_path` returns the path as given; `save_image` encodes `format`). Same for annotate and
   scroll capture.
3. `encode_png_bounded` (lib.rs:444): `max_edge = 0` or an extreme aspect ratio rounds a side to 0 and the encode
   fails. `max_edge` comes straight from API arguments (`actions.rs:258`) with no bounds.
4. The detached `pin` helper (lib.rs:376) is never waited for: a long-running server accumulates zombies.
5. Cancelled selections are detected by matching the string `"selection cancelled"` (`bugle-app/src/main.rs:206,282`).
6. Capture rects are not validated: NaN/infinite/huge rects go to the platform layer (`capture_area` only rejects
   `w <= 0 || h <= 0`, which NaN passes).
7. `capture.rs:154`: `hide_cursor` defaults to hidden with no config; fine, but the `AllScreens` canvas size
   (capture.rs:101-103) is unchecked and each display is captured even if one fails (first error aborts all).
8. `resolve_media` prefers a file in the working directory over a history id with the same name; `subtitles` with an
   explicit `output` does not create the parent directory; `delete_media`/`history` do blocking file IO on the async
   executor.
9. CLI: `bugle scroll --step -8` is rejected by clap (negative value looks like a flag); `record stop` / `record
   status` exist but can never do anything useful in a fresh process; `config show` prints `api.token` and provider
   `api_key` values in the clear.

## Wanted

- One place decides the OCR engine: request value, else config default. `vision` → Apple Vision, anything else → the
  AI path. Both front ends go through it. Result shape stays as documented in `docs/AGENTS.md`.
- When `output` has a recognised image extension and the request gives no `format`, encode in that format. When both
  are given and disagree, the explicit `format` wins and the extension is corrected (and logged).
- Clamp `max_edge` to a sane range (for example 16..=8192) and never produce a 0-pixel side.
- Reap the pin helper (a detached thread that waits, or double-spawn).
- Add `BugleError::Cancelled` ("selection cancelled"), return it from the capture/record paths, map it to 409 in
  `status_for`, and have the app match on the variant.
- Validate rects with `BugleError::Invalid` before calling the platform layer; cap the `AllScreens` canvas.
- History id beats a relative path of the same name (absolute paths and paths containing `/` still mean files).
- Create the parent directory for an explicit subtitles `output`.
- `allow_negative_numbers` (or equivalent) for `--step`; make `record stop`/`record status` either work or say
  clearly what they are (keep the existing message if you cannot do better); redact secrets in `config show`
  (show `"***"`), with a `--reveal` flag.
- Tests for the pure parts (format inference, bounded encode, engine selection, rect validation).

## Out of scope

`record.rs` (002), `scroll.rs`/`stitch.rs` (005), `http.rs` (001), `mcp.rs` (008), `history.rs` (003).
`record.rs` keeps returning `anyhow!("selection cancelled")` until 002 lands; match that message as a fallback in the
app so both orders of merging work.
