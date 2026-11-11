# 007 — Annotation renderer: stray dot in ellipses, panics on hostile documents

Status: closed (fixed on `audit/fixes`)
Severity: medium
Area: `crates/bugle-markup/src/{render.rs,model.rs,lib.rs}`

## Problems

1. **Ellipses with odd width and height get a dot in the middle.** `sdf_ellipse` (render.rs:259-273) returns
   `(k - 1) * k / g`, which is `0` at the exact centre (`k = 0`). A pixel centre lands there when the rect has odd
   dimensions (or a `.5` origin). `coverage(0)` is 0.5, so a filled ellipse has a half-covered centre pixel, and a
   stroked one draws the stroke colour at the centre (`|0| - w/2 < 0`). The approximation is also poor well inside
   thin ellipses.
2. **Annotation documents come from API callers and several fields can panic or exhaust memory:**
   - `canvas.padding` / `canvas.border_width` (u32): `iw + 2 * (pad + bw)` overflows (render.rs:536), or allocates
     gigabytes.
   - `scale`: unbounded resize (render.rs:56-61).
   - `redact.strength` as blur sigma: `pad = (sigma * 3).ceil() as u32`, then `x0 + w + pad` overflows
     (render.rs:486-490); a large sigma also makes `imageops::blur` take minutes.
   - `font_size`, `stroke_width`, `radius`, `dim`: NaN, negative or huge values are not rejected.
   - `image.scale`: unbounded; `image.path` reads any image file the process can open.
   - A document with millions of shapes or freehand points.
3. `spotlight` and `highlight` ignore `opacity`; `Highlight` multiplies with full strength regardless of the colour's
   alpha.
4. `load_font` tells the user to set `annotate.font_path`; the config section is `[markup]`.
5. Doc comments in `model.rs` (lines 101-131 and 210) carry parenthesised identifiers from another codebase. They mean
   nothing to a reader of this project and must go. Describe each shape in this project's own words; do not repeat
   those identifiers anywhere, including commit messages.

## Wanted

- A correct signed distance (or a robust approximation that is strictly negative throughout the interior) for
  ellipses, with a test that renders odd-sized filled and stroked ellipses and checks the centre pixel.
- `Annotation::validate()` called at the top of `render`: finite numbers everywhere, and documented limits
  (for example padding and border ≤ 2048, output ≤ 100 megapixels after canvas and scale, scale in 0.05..=8,
  blur sigma ≤ 100, font size 4..=512, stroke width 0..=256, ≤ 10 000 shapes, ≤ 100 000 freehand points). Violations
  are errors that name the field, and they must surface as HTTP 400: return an error type the API layer can map, or
  document in the commit what `actions.rs` needs (another issue owns that file; a one-line follow-up is fine).
- Checked arithmetic in `apply_canvas` and `blur_region` regardless of validation.
- `opacity` honoured by highlight; highlight strength follows the colour's alpha.
- Correct config key in the font error.
- Clean doc comments.
- Keep `schema_description()` accurate if any field semantics change.
- Tests for each rejection and for the arithmetic paths.

## Out of scope

Everything outside `bugle-markup`.
