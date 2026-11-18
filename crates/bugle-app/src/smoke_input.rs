//! `--smoke-test` steps that drive the windows through AppKit rather than
//! by injecting events at the Rust callback: synthetic mouse and key
//! events built in the process and handed to the window
//! (`bugle_platform::smoke_input`), so the views' own hit-testing, key
//! handling and focus are what is tested. Each step checks the state the
//! window reported back to Rust (playhead, in and out points, cuts, shapes,
//! the prompt's answer) and prints `ok` or `FAILED` with what it saw; a step
//! that cannot run counts as failed.
//!
//! These events reach the window without the window server: the real
//! pointer never moves, no Accessibility permission is needed, and other
//! apps see none of them. The one exception is the opt-in paste step
//! (`BUGLE_SMOKE_CLIPBOARD=1`), which goes through the general pasteboard.
//! A person's input also goes through the window server first (which window
//! is under the pointer, activation, the keyboard layout); that part is not
//! covered.

use crate::editor_window::{self, EditorMsg};
use crate::video_editor::{CLICK_ZOOMS, PRESETS};
use crate::video_editor_window::{self, VideoEditorMsg};
use crate::{AppEvent, Proxy};
use bugle_markup::Shape;
use bugle_platform::smoke_input::{self as input, Key, Mods, Paste, PopupPick, Target, VideoEditorPart};
use image::RgbaImage;
use std::fmt::Display;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Run every input step and print a tally. `true` when every check passed
/// (a step that could not run, or panicked, is a failed check).
pub(crate) fn smoke(proxy: &Proxy) -> bool {
    let mut checks = Checks::default();
    type Step = fn(&Proxy, &mut Checks);
    let steps: [(&str, Step); 3] = [("video editor", video_editor), ("markup editor", markup_editor), ("prompt", prompt)];
    for (name, step) in steps {
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| step(proxy, &mut checks)));
        if let Err(panic) = run {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            checks.check(&format!("{name} step"), false, format!("panicked: {msg}"));
        }
    }
    eprintln!(
        "smoke test: input: {} of {} checks passed{}",
        checks.passed,
        checks.passed + checks.failed,
        if checks.failed > 0 { " (FAILED)" } else { "" }
    );
    checks.failed == 0
}

#[derive(Default)]
struct Checks {
    passed: usize,
    failed: usize,
}

impl Checks {
    fn check(&mut self, what: &str, ok: bool, saw: impl Display) {
        if ok {
            self.passed += 1;
        } else {
            self.failed += 1;
        }
        eprintln!("smoke test: input: {what}: {} ({saw})", if ok { "ok" } else { "FAILED" });
    }

    /// A step that could not run: a failed check.
    fn skipped(&mut self, what: &str, why: impl Display) {
        self.check(what, false, format!("skipped: {why}"));
    }
}

/// Ask `f` every 50 ms until it gives something or `within` is over.
fn poll<T>(within: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let t0 = Instant::now();
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if t0.elapsed() >= within {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The video editor on a synthetic recording (3 s, clicks at 1 s and 2 s):
/// the timeline (click to seek, a click next to a click marker snaps onto
/// it and a drag does not, the in and out handles); the keys Space, I, O, C,
/// Delete, forward delete, the arrows, Home, End and Esc; the Set In, Set
/// Out, Cut, Restore and play buttons and the Smooth pointer check box; the
/// preset, zoom and click-zoom menus; and ⌘W, or ⌘E with
/// `BUGLE_SMOKE_SAVE` (which exports). The Export button is not clicked.
fn video_editor(proxy: &Proxy, c: &mut Checks) {
    const T: Target = Target::VideoEditor;
    let path = std::env::temp_dir().join("bugle-smoke-input.mp4");
    let side = bugle_core::EventTrack::sidecar_path(&path);
    if let Err(e) = video_editor_window::write_input_smoke_recording(&path, &side) {
        return c.skipped("video editor", format!("{e:#}"));
    }
    let active = || input::on_main(bugle_platform::video_editor_active);
    let cleanup = || {
        proxy.send(AppEvent::VideoEditor(VideoEditorMsg::Close));
        poll(Duration::from_secs(2), || (!active()).then_some(()));
        for p in [&path, &side] {
            if let Err(e) = std::fs::remove_file(p) {
                eprintln!("smoke test: could not remove {} ({e})", p.display());
            }
        }
    };
    proxy.send(AppEvent::VideoEditor(VideoEditorMsg::Open(path.display().to_string())));
    let state = || input::on_main(video_editor_window::smoke_state);
    let rect = input::video_editor_rect;
    // Open, with its click markers (the state's first answer has them).
    let Some((track, s0)) = poll(Duration::from_secs(5), || Some((rect(VideoEditorPart::Track)?, state()?))) else {
        c.skipped("video editor", "no video editor window after 5s");
        return cleanup();
    };
    let d = s0.out_point;
    let x = |t: f64| track.x + track.w * t / d;
    let time_at = |x: f64| ((x - track.x) / track.w).clamp(0.0, 1.0) * d;
    let y = track.y + track.h / 2.0;
    // A point and a half of the timeline, in seconds.
    let tol = 1.5 * d / track.w;
    let near = |a: f64, b: f64| (a - b).abs() <= tol;
    let click_at = |t: f64| {
        input::mouse_down(T, (x(t), y));
        input::mouse_up(T, (x(t), y));
    };
    let key = |k: Key| input::key(T, k, Mods::NONE);
    let fmt = |s: &bugle_platform::VideoEditorState| {
        format!(
            "playhead {:.3}, in {:.3}, out {:.3}, cuts {:?}, mark {:?}, playing {}",
            s.playhead, s.in_point, s.out_point, s.cuts, s.cut_mark, s.playing
        )
    };
    eprintln!("smoke test: input: video editor track at {track:?} for {d:.2}s, key window {:?}", bugle_platform::key_window_title());

    // ---- timeline ----
    click_at(0.75);
    let s = state().unwrap_or_default();
    c.check("timeline click seeks to 0.75s", near(s.playhead, 0.75), fmt(&s));
    let marker = s0.clicks.first().map_or(1.0, |m| m.t);
    let beside = x(marker) + 3.0;
    input::mouse_down(T, (beside, y));
    input::mouse_up(T, (beside, y));
    let s = state().unwrap_or_default();
    c.check(&format!("a click 3 pt from the marker at {marker:.2}s snaps onto it"), (s.playhead - marker).abs() < 1e-9, fmt(&s));
    input::drag(T, (x(0.5), y), (beside, y), 4);
    let s = state().unwrap_or_default();
    c.check(
        "a drag ending 3 pt from the marker does not snap",
        near(s.playhead, time_at(beside)) && (s.playhead - marker).abs() > 1e-6,
        format!("{} (want {:.3})", fmt(&s), time_at(beside)),
    );
    let s = state().unwrap_or_default();
    input::drag(T, (x(s.in_point) - 2.5, y), (x(0.5), y), 4);
    let s = state().unwrap_or_default();
    c.check("dragging the in handle to 0.5s", near(s.in_point, 0.5) && (s.out_point - d).abs() < 1e-9, fmt(&s));
    input::drag(T, (x(s.out_point) + 2.5, y), (x(2.5), y), 4);
    let s = state().unwrap_or_default();
    c.check("dragging the out handle to 2.5s", near(s.out_point, 2.5) && near(s.in_point, 0.5), fmt(&s));

    // ---- keys ----
    click_at(1.5);
    let route = key(Key::Char('i'));
    let s = state().unwrap_or_default();
    c.check(&format!("I sets the in point at the playhead (key route {route:?})"), near(s.in_point, 1.5) && s.in_point == s.playhead, fmt(&s));
    let before = s.playhead;
    let step = 1.0 / 30.0;
    key(Key::Right);
    let s = state().unwrap_or_default();
    c.check("→ steps one frame forward", (s.playhead - before - step).abs() < 1e-6, fmt(&s));
    key(Key::Left);
    let s = state().unwrap_or_default();
    c.check("← steps one frame back", (s.playhead - before).abs() < 1e-6, fmt(&s));
    key(Key::Down);
    let s = state().unwrap_or_default();
    c.check("↓ goes to the out point", s.playhead == s.out_point, fmt(&s));
    key(Key::Up);
    let s = state().unwrap_or_default();
    c.check("↑ goes to the in point", s.playhead == s.in_point, fmt(&s));
    key(Key::End);
    let s = state().unwrap_or_default();
    c.check("End goes to the out point", s.playhead == s.out_point, fmt(&s));
    key(Key::Home);
    let s = state().unwrap_or_default();
    c.check("Home goes to the in point", s.playhead == s.in_point, fmt(&s));
    click_at(2.2);
    key(Key::Char('o'));
    let s = state().unwrap_or_default();
    c.check("O sets the out point at the playhead", near(s.out_point, 2.2) && s.out_point == s.playhead, fmt(&s));
    key(Key::Char(' '));
    let s = state().unwrap_or_default();
    c.check("Space plays", s.playing, fmt(&s));
    key(Key::Char(' '));
    let s = state().unwrap_or_default();
    // Paused, not played out (the clip is 0.7s; the out point would mean it ended).
    c.check("Space again pauses", !s.playing && s.playhead < s.out_point, fmt(&s));
    key(Key::Char(' '));
    let playing = state().is_some_and(|s| s.playing);
    key(Key::Escape);
    let s = state().unwrap_or_default();
    c.check("Esc pauses playback", playing && !s.playing, format!("playing before Esc: {playing}; {}", fmt(&s)));
    click_at(1.7);
    key(Key::Char('c'));
    let s = state().unwrap_or_default();
    c.check("C starts a cut at the playhead", s.cut_mark.is_some_and(|m| near(m, 1.7)), fmt(&s));
    click_at(1.9);
    key(Key::Char('c'));
    let s = state().unwrap_or_default();
    let one_cut = matches!(s.cuts.as_slice(), [(a, b)] if near(*a, 1.7) && near(*b, 1.9));
    c.check("C again ends it: one cut 1.7–1.9s", one_cut && s.cut_mark.is_none(), fmt(&s));
    click_at(1.8);
    key(Key::Delete);
    let s = state().unwrap_or_default();
    c.check("Delete restores the cut under the playhead", s.cuts.is_empty(), fmt(&s));
    key(Key::Char('c'));
    let marked = state().is_some_and(|s| s.cut_mark.is_some());
    key(Key::ForwardDelete);
    let s = state().unwrap_or_default();
    c.check("forward delete drops the cut being marked", marked && s.cut_mark.is_none() && s.cuts.is_empty(), fmt(&s));
    key(Key::Char('c'));
    let marked = state().is_some_and(|s| s.cut_mark.is_some());
    key(Key::Escape);
    let s = state().unwrap_or_default();
    c.check("Esc drops the cut being marked", marked && s.cut_mark.is_none() && s.cuts.is_empty(), fmt(&s));

    // ---- buttons and menus ----
    click_at(1.2);
    let route = rect(VideoEditorPart::SetIn).and_then(|r| input::click(T, r.mid()));
    let s = state().unwrap_or_default();
    c.check(&format!("the Set In button (mouse-up {route:?})"), route.is_some() && near(s.in_point, 1.2), fmt(&s));
    let button = |part: VideoEditorPart| rect(part).and_then(|r| input::click(T, r.mid()));
    click_at(2.1);
    let route = button(VideoEditorPart::SetOut);
    let s = state().unwrap_or_default();
    c.check(&format!("the Set Out button (mouse-up {route:?})"), route.is_some() && near(s.out_point, 2.1), fmt(&s));
    click_at(1.4);
    button(VideoEditorPart::Cut);
    let marked = state().and_then(|s| s.cut_mark);
    click_at(1.6);
    let route = button(VideoEditorPart::Cut);
    let s = state().unwrap_or_default();
    let one_cut = matches!(s.cuts.as_slice(), [(a, b)] if near(*a, 1.4) && near(*b, 1.6));
    c.check(
        &format!("the Cut button, twice: one cut 1.4–1.6s (mouse-up {route:?})"),
        marked.is_some_and(|m| near(m, 1.4)) && one_cut,
        format!("mark after the first {marked:?}; {}", fmt(&s)),
    );
    click_at(1.5);
    let route = button(VideoEditorPart::Restore);
    let s = state().unwrap_or_default();
    c.check(&format!("the Restore button restores it (mouse-up {route:?})"), one_cut && s.cuts.is_empty(), fmt(&s));
    let route = rect(VideoEditorPart::Play).and_then(|r| input::click(T, r.mid()));
    let s = state().unwrap_or_default();
    c.check(&format!("the play button plays (mouse-up {route:?})"), s.playing, fmt(&s));
    // Esc only ever pauses (Space would start it again had it played out).
    key(Key::Escape);
    let smooth = state().is_some_and(|s| s.smooth);
    let route = rect(VideoEditorPart::Smooth).and_then(|r| input::click(T, r.mid()));
    let s = state().unwrap_or_default();
    c.check(&format!("the Smooth pointer check box (mouse-up {route:?})"), route.is_some() && s.smooth != smooth, format!("smooth {smooth} → {}", s.smooth));
    let p720 = PRESETS.iter().position(|p| *p == bugle_engine::ExportPreset::P720).unwrap_or(0);
    let pick = rect(VideoEditorPart::PresetMenu).and_then(|r| input::pick_popup(T, r.mid(), p720));
    let s = state().unwrap_or_default();
    c.check(&format!("the preset menu opens and 720p is chosen ({pick:?})"), pick == Some(PopupPick::Opened) && s.preset == p720, format!("preset {} (want {p720})", s.preset));
    let pick = rect(VideoEditorPart::ZoomMenu).and_then(|r| input::pick_popup(T, r.mid(), 2));
    let s = state().unwrap_or_default();
    c.check(&format!("the zoom menu opens and 2× is chosen ({pick:?})"), pick == Some(PopupPick::Opened) && s.zoom == 2, format!("zoom {} (want 2)", s.zoom));
    // The click menu is about the click at the playhead: snap onto the
    // second marker, then give that click 3×.
    let second = s0.clicks.get(1).map_or(2.0, |m| m.t);
    input::mouse_down(T, (x(second) + 2.0, y));
    input::mouse_up(T, (x(second) + 2.0, y));
    let three = CLICK_ZOOMS.iter().position(|z| z.1 == Some(3.0)).unwrap_or(0);
    let pick = rect(VideoEditorPart::ClickMenu).and_then(|r| input::pick_popup(T, r.mid(), three));
    let s = state().unwrap_or_default();
    let level = s.clicks.get(1).map(|m| m.level);
    c.check(
        &format!("the click menu opens and 3× is chosen for the click at {second:.2}s ({pick:?})"),
        pick == Some(PopupPick::Opened) && s.click_zoom == Some(three) && level == Some(3.0),
        format!("playhead {:.3}, click choice {:?} (want Some({three})), level {level:?}", s.playhead, s.click_zoom),
    );
    // The buttons and menus never take the keyboard: keys still reach the
    // timeline.
    let before = s.playhead;
    key(Key::Right);
    let s = state().unwrap_or_default();
    c.check("after the buttons and menus → still steps a frame", (s.playhead - before - step).abs() < 1e-6, fmt(&s));

    // ---- closing ----
    let t0 = Instant::now();
    if std::env::var_os("BUGLE_SMOKE_SAVE").is_some() {
        let route = input::key(T, Key::Char('e'), Mods::COMMAND);
        let closed = poll(Duration::from_secs(15), || (!active()).then_some(())).is_some();
        c.check(&format!("⌘E exports, and the window closes when written (key route {route:?})"), closed, format!("after {:.1}s", t0.elapsed().as_secs_f64()));
    } else {
        let route = input::key(T, Key::Char('w'), Mods::COMMAND);
        let closed = poll(Duration::from_secs(2), || (!active()).then_some(())).is_some();
        c.check(&format!("⌘W closes the window (key route {route:?})"), closed, format!("after {:.1}s", t0.elapsed().as_secs_f64()));
    }
    cleanup();
}

/// The markup editor on a synthetic image: pick the rectangle tool with its
/// key, drag one out on the canvas, undo it with ⌘Z and redo it with ⇧⌘Z,
/// then type a two-line label (⇧Return for the new line, Return to finish).
fn markup_editor(proxy: &Proxy, c: &mut Checks) {
    const T: Target = Target::MarkupEditor;
    let img = RgbaImage::from_fn(600, 400, |x, y| image::Rgba([30 + (x / 5) as u8, 90 + (y / 4) as u8, 150, 255]));
    let path = std::env::temp_dir().join("bugle-smoke-input.png");
    if let Err(e) = img.save(&path) {
        return c.skipped("markup editor", e);
    }
    let shapes = || input::on_main(editor_window::smoke_shapes);
    let cleanup = || {
        proxy.send(AppEvent::Editor(EditorMsg::Close));
        poll(Duration::from_secs(2), || shapes().is_none().then_some(()));
        if let Err(e) = std::fs::remove_file(&path) {
            eprintln!("smoke test: could not remove {} ({e})", path.display());
        }
    };
    proxy.send(AppEvent::Editor(EditorMsg::Open(path.display().to_string())));
    let Some(before) = poll(Duration::from_secs(5), || {
        input::markup_editor_point(0.0, 0.0)?;
        shapes()
    }) else {
        c.skipped("markup editor", "no markup editor window after 5s");
        return cleanup();
    };
    let rects = |v: &Option<Vec<Shape>>| match v {
        Some(v) => format!(
            "{} shape(s): {:?}",
            v.len(),
            v.iter()
                .map(|s| match s {
                    Shape::Rectangle { rect, .. } => format!("rectangle {:.0},{:.0} {:.0}×{:.0}", rect.x, rect.y, rect.w, rect.h),
                    Shape::Text { text, .. } => format!("text {text:?}"),
                    _ => "other".to_string(),
                })
                .collect::<Vec<_>>()
        ),
        None => "no editor".into(),
    };
    c.check("markup editor opens empty", before.is_empty(), rects(&Some(before.clone())));
    let route = input::key(T, Key::Char('r'), Mods::NONE);
    let (from, to) = (input::markup_editor_point(100.0, 80.0), input::markup_editor_point(320.0, 260.0));
    if let (Some(from), Some(to)) = (from, to) {
        input::drag(T, from, to, 6);
    }
    let drawn = shapes();
    // Within a pixel or two of where it was aimed: the drag is in points.
    let close = |a: f64, b: f64| (a - b).abs() <= 2.5;
    let ok = matches!(drawn.as_deref(), Some([Shape::Rectangle { rect, .. }])
        if close(rect.x, 100.0) && close(rect.y, 80.0) && close(rect.w, 220.0) && close(rect.h, 180.0));
    c.check(&format!("R and a drag on the canvas draw a rectangle at 100,80 220×180 (key route {route:?})"), ok, rects(&drawn));
    let route = input::key(T, Key::Char('z'), Mods::COMMAND);
    let undone = shapes();
    c.check(&format!("⌘Z undoes it (key route {route:?})"), undone.as_ref().is_some_and(|v| v.is_empty()), rects(&undone));
    input::key(T, Key::Char('z'), Mods::COMMAND.with(Mods::SHIFT));
    let redone = shapes();
    c.check("⇧⌘Z redoes it", redone.as_ref().is_some_and(|v| v.len() == 1), rects(&redone));
    // A label: T, a click opens the text field, ⇧Return starts a second
    // line (the window reads the modifiers from NSApp.currentEvent), Return
    // finishes it.
    input::key(T, Key::Char('t'), Mods::NONE);
    if let Some(at) = input::markup_editor_point(120.0, 330.0) {
        input::click(T, at);
    }
    input::type_text(T, "Hi");
    input::key(T, Key::Return, Mods::SHIFT);
    input::type_text(T, "there");
    input::key(T, Key::Return, Mods::NONE);
    let labelled = shapes();
    let label = labelled.as_ref().and_then(|v| {
        v.iter().find_map(|s| match s {
            Shape::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
    });
    c.check(
        "T, a click, typing, ⇧Return, typing and Return add the two-line label \"Hi\\nthere\"",
        label.as_deref() == Some("Hi\nthere") && labelled.as_ref().is_some_and(|v| v.len() == 2),
        format!("label {label:?}, {}", rects(&labelled)),
    );
    cleanup();
}

/// The text prompt (the one *Audio Levels → Custom…* opens), opened here
/// with an answer that goes nowhere, so nothing is written to the config:
/// typing over the selected value, ⌘A then typing, (with
/// `BUGLE_SMOKE_CLIPBOARD=1`) ⌘A then ⌘V, and Return give the typed text;
/// typing and Esc cancel.
fn prompt(_proxy: &Proxy, c: &mut Checks) {
    const T: Target = Target::Prompt;
    let field = input::prompt_field_text;
    let open = |initial: &'static str| {
        // The previous prompt is gone first.
        poll(Duration::from_secs(2), || (!input::on_main(bugle_platform::prompt_active)).then_some(()))?;
        let (tx, rx) = mpsc::channel::<Option<String>>();
        let opened = input::on_main(move || {
            bugle_platform::prompt_text("Smoke Test Level", "Type a level.", initial, move |answer| {
                let _ = tx.send(answer);
            })
        });
        // Up, with its field being edited (the value selected).
        (opened && poll(Duration::from_secs(2), || field().filter(|t| t == initial)).is_some()).then_some(rx)
    };
    let Some(rx) = open("100") else {
        return c.skipped("prompt", "it did not open (another prompt is open?)");
    };
    eprintln!("smoke test: input: prompt key window {:?}", bugle_platform::key_window_title());
    // The value starts selected, so the first key replaces it; ⌘A must then
    // select what was typed for the next key to replace it in turn.
    input::type_text(T, "8");
    let first = field();
    let route = input::key(T, Key::Char('a'), Mods::COMMAND);
    input::type_text(T, "1");
    let typed = field();
    c.check(
        &format!("prompt: typing replaces the selected value, then ⌘A and typing replace that (key route {route:?})"),
        first.as_deref() == Some("8") && typed.as_deref() == Some("1"),
        format!("{first:?} then {typed:?}"),
    );
    input::key(T, Key::Char('a'), Mods::COMMAND);
    if std::env::var_os("BUGLE_SMOKE_CLIPBOARD").is_some() {
        let paste = input::paste(T, "37");
        let pasted = field();
        match paste {
            Some(Paste::Declined) => {
                eprintln!("smoke test: input: prompt: ⌘V skipped (the clipboard holds more than plain text; left untouched)");
                input::type_text(T, "37");
            }
            _ => c.check(
                &format!("prompt: ⌘A then ⌘V replaces it with the clipboard ({paste:?})"),
                pasted.as_deref() == Some("37") && paste == Some(Paste::Restored),
                format!("{pasted:?}"),
            ),
        }
    } else {
        eprintln!("smoke test: input: prompt: ⌘V skipped (set BUGLE_SMOKE_CLIPBOARD=1 to paste through the real clipboard)");
        input::type_text(T, "37");
    }
    input::type_text(T, "5");
    let before_return = field();
    input::key(T, Key::Return, Mods::NONE);
    let answer = rx.recv_timeout(Duration::from_secs(2));
    c.check(
        "prompt: Return answers \"375\"",
        matches!(&answer, Ok(Some(t)) if t == "375"),
        format!("{answer:?}, field before Return {before_return:?}"),
    );
    if answer.is_err() {
        bugle_platform::prompt_answer(None);
    }
    let Some(rx) = open("100") else {
        return c.skipped("prompt (second)", "it did not open");
    };
    input::type_text(T, "9");
    input::key(T, Key::Escape, Mods::NONE);
    let answer = rx.recv_timeout(Duration::from_secs(2));
    c.check("prompt: typing then Esc cancels", matches!(answer, Ok(None)), format!("{answer:?}"));
    let gone = poll(Duration::from_secs(2), || (!input::on_main(bugle_platform::prompt_active)).then_some(())).is_some();
    c.check("no prompt left open", gone, format!("closed: {gone}"));
    if !gone {
        bugle_platform::prompt_answer(None);
    }
}
