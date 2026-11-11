# 005 — Scrolling capture: panics, unbounded canvas, quadratic stitching

Status: closed (fixed on `audit/fixes`)
Severity: medium
Area: `crates/bugle-engine/src/{scroll.rs,stitch.rs}`

## Problems

1. `estimate_shift_profile` (stitch.rs:83-86): for a profile of exactly 8 samples, `n = 7` and `min_overlap = 8`, so
   `n - min_overlap` underflows (panic in debug, a huge loop then an out-of-bounds panic in release). Small scroll
   rects reach this.
2. `max_frames`, `step_lines` and `settle_ms` come from API arguments unbounded. `max_frames = 4_000_000_000` on a
   page that keeps scrolling grows the canvas until the process is killed.
3. `append_strip` reallocates and copies the whole canvas for every frame (quadratic in the number of frames).
4. When Vision and the profile estimate disagree, the code only logs (scroll.rs:113-117) and always trusts Vision,
   even when the profile estimate is confident and Vision is far off (the sticky-header case the comment describes).
5. `frame_difference` samples only every `step`-th pixel of the R and G channels starting at index 0, so a change
   confined to a small region can be missed and counted as a stall.
6. The view is left scrolled to the bottom; `scroll_back` exists but nothing calls it.
7. Frames whose size differs from the first frame (window resized mid-capture) are stitched anyway.
8. `step_lines = 0` loops until `max_frames` doing nothing useful ("end of content" after two stalls, but only by luck).

## Wanted

- No arithmetic that can underflow; return a zero-confidence shift for profiles too short to compare.
- Clamp request values (`max_frames` 2..=500, `settle_ms` 50..=5000, `step_lines` non-zero and within ±100) with
  `BugleError::Invalid` for nonsense, and cap the stitched canvas (for example 200 megapixels) with a
  `stopped_reason` of `"canvas limit"`.
- Collect strips and assemble the canvas once at the end (or grow geometrically).
- Use the profile estimate when its confidence is high and Vision disagrees by more than a few pixels. Keep the
  existing unit tests passing and add one for the disagreement rule (pure function over the two estimates).
- Scroll back by the number of steps taken when the capture ends, successfully or not.
- Stop with a clear `stopped_reason` when the frame size changes.
- Tests for the underflow case, clamping and one-shot assembly.

## Notes

Accessibility is not granted on the development Mac, so the end-to-end scroll cannot be exercised; keep the logic in
pure functions with unit tests.

## Out of scope

Everything outside these two files.
