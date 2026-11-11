# 003 — Core: history deletes the wrong item, naming and config drift

Status: open
Severity: high (item 1), otherwise low
Area: `crates/bugle-core/src/{history.rs,naming.rs,geometry.rs,config.rs}`, `config.example.toml`

## Problems

1. **`History::get` matches any prefix, including the empty string.** `history.rs:89` uses
   `m.id == id || m.id.starts_with(id)` and returns the newest match. So `delete_media {"id": "", "delete_file": true}`
   (or `bugle delete "" --file`, or a one-character prefix) removes the newest capture and its file. Ambiguous prefixes
   silently pick one.
2. **`push` writes a record in several `write` calls** (`serde_json::to_writer` straight to the file, then `\n`). The CLI,
   the app and the server can all append to the same file, so lines can interleave and corrupt each other.
3. Reading history is quadratic (`items.iter().position` per line, `deleted.contains` per item).
4. The file only grows: updates and tombstones are never compacted.
5. `naming::render`: an unbalanced `{` in a window title or label swallows the rest of the file name (the cleanup loop
   at naming.rs:35-44 runs over substituted values too). `{counter}` is documented at the top of the file and in
   `config.example.toml` but not implemented. A rendered name can be empty, giving `.png`.
6. `Rect::parse` accepts `nan`/`inf` and splits on `x`, so `1e3`-style or hex-looking input gives surprising results.
7. Config fields that nothing reads: `screenshots.desktop_icons_hidden`, `screenshots.self_timer_seconds`,
   `markup.padding`, `markup.corner_radius`, `markup.shadow`. They are in `config.example.toml` as if they worked.

## Wanted

- `get` / `delete`: exact id wins; otherwise a prefix of at least 4 characters that matches exactly one live item.
  Ambiguous prefix → `BugleError::Invalid` listing the candidates; empty/too-short → `Invalid`. Keep the
  `Result<Option<MediaItem>>` shape so callers compile unchanged.
- `push`: serialise to one buffer including the newline and write it with a single `write_all`.
- Linear-time read (index by id).
- Compact on `open` when the file has more dead lines than live ones (write to a temp file in the same directory and
  rename). Must be safe if another process appends concurrently: skip compaction rather than risk losing a line if you
  cannot make it safe, and say so in a comment.
- `render`: strip unresolved `{token}`s from the template before substituting values, so braces inside values survive
  (sanitised). Implement `{counter}` or remove it from the docs (your call; say which in the commit). Fall back to
  `Bugle` plus the timestamp when the result is empty.
- `Rect::parse`: finite numbers only; accept `x,y,w,h` and `x,y,WxH`; add `Rect::is_valid()` (finite, non-empty).
- For each unused config field: wire it up if that is a few lines in this crate, otherwise remove it from the struct
  and from `config.example.toml`. Unknown keys in existing user configs must keep parsing (serde ignores them today;
  keep it that way). `markup.*` canvas defaults are read by the engine in another issue's files, so for those just
  remove or leave with an accurate doc comment.
- Tests for each of the above.

## Out of scope

`ApiConfig` (issue 001), anything outside `bugle-core` and `config.example.toml`.
