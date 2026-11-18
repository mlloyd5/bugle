//! `--smoke-test` steps that drive the windows through AppKit rather than
//! by injecting events at the Rust callback: synthetic mouse and key
//! events built in the process and handed to the window
//! (`bugle_platform::smoke_input`), so the views' own hit-testing, key
//! handling and focus are what is tested. Each step checks the state the
//! window reported back to Rust (playhead, in and out points, cuts, shapes,
//! the prompt's answer) and prints `ok` or `FAILED` with what it saw.
//!
//! These events reach the window without the window server: the real
//! pointer never moves, other apps see nothing, and no Accessibility
//! permission is needed. A person's input also goes through the window
//! server first (which window is under the pointer, activation, the
//! keyboard layout); that part is not covered.

use crate::editor_window::{self, EditorMsg};
use crate::video_editor::PRESETS;
use crate::video_editor_window::{self, VideoEditorMsg};
use crate::{AppEvent, Proxy};
use bugle_markup::Shape;
use bugle_platform::smoke_input::{self as input, Key, Mods, Target, VideoEditorPart};
use image::RgbaImage;
use std::fmt::Display;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Run every input step and print a tally.
pub(crate) fn smoke(proxy: &Proxy) {
    let mut checks = Checks::default();
    video_editor(proxy, &mut checks);
    markup_editor(proxy, &mut checks);
    prompt(&mut checks);
    eprintln!(
        "smoke test: input: {} of {} checks passed{}",
        checks.passed,
        checks.passed + checks.failed,
        if checks.failed > 0 { " (FAILED)" } else { "" }
    );
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
}

/// The video editor on a synthetic recording (3 s, clicks at 1 s and 2 s):
/// the timeline (click to seek, a click next to a click marker snaps onto
/// it and a drag does not, the in and out handles), every keyboard
/// shortcut, the Set In and play buttons, the Smooth pointer check box, the
/// preset and zoom menus, and ⌘W (⌘E with `BUGLE_SMOKE_SAVE`, which exports).
fn video_editor(proxy: &Proxy, c: &mut Checks) {
    const T: Target = Target::VideoEditor;
    let path = std::env::temp_dir().join("bugle-smoke-input.mp4");
    let side = bugle_core::EventTrack::sidecar_path(&path);
    if let Err(e) = video_editor_window::write_smoke_recording(&path, &side) {
        eprintln!("smoke test: input: video editor skipped ({e:#})");
        return;
    }
    let cleanup = || {
        proxy.send(AppEvent::VideoEditor(VideoEditorMsg::Close));
        std::thread::sleep(Duration::from_millis(300));
        for p in [&path, &side] {
            if let Err(e) = std::fs::remove_file(p) {
                eprintln!("smoke test: could not remove {} ({e})", p.display());
            }
        }
    };
    proxy.send(AppEvent::VideoEditor(VideoEditorMsg::Open(path.display().to_string())));
    std::thread::sleep(Duration::from_millis(900));
    let state = || input::on_main(video_editor_window::smoke_state);
    let rect = input::video_editor_rect;
    let (Some(track), Some(s0)) = (rect(VideoEditorPart::Track), state()) else {
        c.check("video editor open", false, "no video editor window");
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
    c.check("Space again pauses", !s.playing, fmt(&s));
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
    key(Key::Escape);
    let s = state().unwrap_or_default();
    c.check("Esc drops the cut being marked", marked && s.cut_mark.is_none() && s.cuts.is_empty(), fmt(&s));

    // ---- buttons and menus ----
    click_at(1.2);
    let route = rect(VideoEditorPart::SetIn).and_then(|r| input::click(T, r.mid()));
    let s = state().unwrap_or_default();
    c.check(&format!("the Set In button (mouse-up {route:?})"), route.is_some() && near(s.in_point, 1.2), fmt(&s));
    let route = rect(VideoEditorPart::Play).and_then(|r| input::click(T, r.mid()));
    let s = state().unwrap_or_default();
    c.check(&format!("the play button plays (mouse-up {route:?})"), s.playing, fmt(&s));
    key(Key::Char(' '));
    let smooth = state().is_some_and(|s| s.smooth);
    let route = rect(VideoEditorPart::Smooth).and_then(|r| input::click(T, r.mid()));
    let s = state().unwrap_or_default();
    c.check(&format!("the Smooth pointer check box (mouse-up {route:?})"), route.is_some() && s.smooth != smooth, format!("smooth {smooth} → {}", s.smooth));
    let p720 = PRESETS.iter().position(|p| *p == bugle_engine::ExportPreset::P720).unwrap_or(0);
    let pick = rect(VideoEditorPart::PresetMenu).and_then(|r| input::pick_popup(T, r.mid(), p720));
    let s = state().unwrap_or_default();
    c.check(&format!("the preset menu: 720p ({pick:?})"), s.preset == p720, format!("preset {} (want {p720})", s.preset));
    let pick = rect(VideoEditorPart::ZoomMenu).and_then(|r| input::pick_popup(T, r.mid(), 2));
    let s = state().unwrap_or_default();
    c.check(&format!("the zoom menu: 2× ({pick:?})"), s.zoom == 2, format!("zoom {} (want 2)", s.zoom));
    // The buttons and menus never take the keyboard: keys still reach the
    // timeline.
    let before = s.playhead;
    key(Key::Right);
    let s = state().unwrap_or_default();
    c.check("after the buttons and menus → still steps a frame", (s.playhead - before - step).abs() < 1e-6, fmt(&s));

    // ---- closing ----
    let active = || input::on_main(bugle_platform::video_editor_active);
    if std::env::var_os("BUGLE_SMOKE_SAVE").is_some() {
        let route = input::key(T, Key::Char('e'), Mods::COMMAND);
        let t0 = Instant::now();
        while active() && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(200));
        }
        c.check(&format!("⌘E exports, and the window closes when written (key route {route:?})"), !active(), format!("after {:.1}s", t0.elapsed().as_secs_f64()));
    } else {
        let route = input::key(T, Key::Char('w'), Mods::COMMAND);
        std::thread::sleep(Duration::from_millis(200));
        c.check(&format!("⌘W closes the window (key route {route:?})"), !active(), format!("open: {}", active()));
    }
    cleanup();
}

/// The markup editor on a synthetic image: pick the rectangle tool with its
/// key, drag one out on the canvas, undo it with ⌘Z and redo it with ⇧⌘Z.
fn markup_editor(proxy: &Proxy, c: &mut Checks) {
    const T: Target = Target::MarkupEditor;
    let img = RgbaImage::from_fn(600, 400, |x, y| image::Rgba([30 + (x / 5) as u8, 90 + (y / 4) as u8, 150, 255]));
    let path = std::env::temp_dir().join("bugle-smoke-input.png");
    if let Err(e) = img.save(&path) {
        eprintln!("smoke test: input: markup editor skipped ({e})");
        return;
    }
    proxy.send(AppEvent::Editor(EditorMsg::Open(path.display().to_string())));
    std::thread::sleep(Duration::from_millis(900));
    let shapes = || input::on_main(editor_window::smoke_shapes);
    let rects = |v: &Option<Vec<Shape>>| match v {
        Some(v) => format!(
            "{} shape(s): {:?}",
            v.len(),
            v.iter()
                .map(|s| match s {
                    Shape::Rectangle { rect, .. } => format!("rectangle {:.0},{:.0} {:.0}×{:.0}", rect.x, rect.y, rect.w, rect.h),
                    _ => "other".to_string(),
                })
                .collect::<Vec<_>>()
        ),
        None => "no editor".into(),
    };
    let before = shapes();
    c.check("markup editor opens empty", before.as_ref().is_some_and(|v| v.is_empty()), rects(&before));
    let route = input::key(T, Key::Char('r'), Mods::NONE);
    let (from, to) = (input::markup_editor_point(100.0, 80.0), input::markup_editor_point(320.0, 260.0));
    if let (Some(from), Some(to)) = (from, to) {
        input::drag(T, from, to, 6);
    }
    std::thread::sleep(Duration::from_millis(200));
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
    // A label: T, a click opens the text field, typing and Return finish it.
    input::key(T, Key::Char('t'), Mods::NONE);
    if let Some(at) = input::markup_editor_point(120.0, 330.0) {
        input::click(T, at);
    }
    std::thread::sleep(Duration::from_millis(200));
    input::type_text(T, "Hi there");
    input::key(T, Key::Return, Mods::NONE);
    std::thread::sleep(Duration::from_millis(200));
    let labelled = shapes();
    let label = labelled.as_ref().and_then(|v| {
        v.iter().find_map(|s| match s {
            Shape::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
    });
    c.check(
        "T, a click, typing and Return add the label \"Hi there\"",
        label.as_deref() == Some("Hi there") && labelled.as_ref().is_some_and(|v| v.len() == 2),
        format!("label {label:?}, {}", rects(&labelled)),
    );
    proxy.send(AppEvent::Editor(EditorMsg::Close));
    std::thread::sleep(Duration::from_millis(300));
    if let Err(e) = std::fs::remove_file(&path) {
        eprintln!("smoke test: could not remove {} ({e})", path.display());
    }
}

/// The text prompt (the one *Audio Levels → Custom…* opens), opened here
/// with an answer that goes nowhere, so nothing is written to the config:
/// ⌘A over the value, typing, ⌘V and Return give the typed text; typing and
/// Esc cancel. The paste borrows the clipboard, which is put back after.
fn prompt(c: &mut Checks) {
    const T: Target = Target::Prompt;
    let open = |initial: &'static str| {
        let (tx, rx) = mpsc::channel::<Option<String>>();
        let opened = input::on_main(move || {
            bugle_platform::prompt_text("Smoke Test Level", "Type a level.", initial, move |answer| {
                let _ = tx.send(answer);
            })
        });
        std::thread::sleep(Duration::from_millis(500));
        opened.then_some(rx)
    };
    let Some(rx) = open("100") else {
        return c.check("prompt opens", false, "another prompt is open");
    };
    eprintln!("smoke test: input: prompt key window {:?}", bugle_platform::key_window_title());
    let field = input::prompt_field_text;
    let route = input::key(T, Key::Char('a'), Mods::COMMAND);
    input::type_text(T, "1");
    let typed = field();
    c.check(&format!("prompt: ⌘A then typing replaces the value (key route {route:?})"), typed.as_deref() == Some("1"), format!("{typed:?}"));
    input::key(T, Key::Char('a'), Mods::COMMAND);
    input::with_pasteboard_text("37", || input::key(T, Key::Char('v'), Mods::COMMAND));
    let pasted = field();
    c.check("prompt: ⌘A then ⌘V replaces it with the clipboard", pasted.as_deref() == Some("37"), format!("{pasted:?}"));
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
    std::thread::sleep(Duration::from_millis(300));
    let Some(rx) = open("100") else {
        return c.check("prompt opens again", false, "another prompt is open");
    };
    input::type_text(T, "9");
    input::key(T, Key::Escape, Mods::NONE);
    let answer = rx.recv_timeout(Duration::from_secs(2));
    c.check("prompt: typing then Esc cancels", matches!(answer, Ok(None)), format!("{answer:?}"));
    std::thread::sleep(Duration::from_millis(300));
    let left = input::on_main(bugle_platform::prompt_active);
    c.check("no prompt left open", !left, format!("open: {left}"));
    if left {
        bugle_platform::prompt_answer(None);
    }
}
