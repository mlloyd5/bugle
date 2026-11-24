//! Diagnostic for CI: `cargo run -p bugle-platform --example ci_probe`.
//! Prints what this machine reports for permissions, displays and windows,
//! and never prompts: unlike `probe`, it does not request Screen Recording
//! access, and it only captures when access is already granted.
//!
//! With `--require-screen` it exits non-zero when Screen Recording is not
//! granted, there is no display, or the capture fails, so CI notices when
//! the capture tests would quietly skip their capture part.
use bugle_platform as p;

fn main() {
    let require = std::env::args().skip(1).any(|a| a == "--require-screen");
    let mut missing = Vec::new();

    let screen = p::screen_access_preflight();
    println!("screen recording (preflight): {screen}");
    println!("accessibility (no prompt): {}", p::accessibility_trusted(false));
    println!("microphone: {}", p::microphone_access());
    if !screen {
        missing.push("Screen Recording access");
    }
    match p::displays() {
        Ok(ds) => {
            println!("displays: {}", ds.len());
            for d in &ds {
                println!("  display {} main={} frame {} scale {}", d.id, d.is_main, d.frame, d.scale);
            }
            if ds.is_empty() {
                missing.push("a display");
            }
        }
        Err(e) => {
            println!("displays: error: {e:#}");
            missing.push("a display");
        }
    }
    match p::windows(true) {
        Ok(ws) => println!("on-screen windows: {}", ws.len()),
        Err(e) => println!("on-screen windows: error: {e:#}"),
    }
    if screen {
        match p::capture_area(bugle_core::Rect::new(0.0, 0.0, 64.0, 48.0), false) {
            Ok(img) => println!("capture 64x48 pt: {}x{} px", img.width(), img.height()),
            Err(e) => {
                println!("capture 64x48 pt: error: {e:#}");
                missing.push("a working capture");
            }
        }
    } else {
        println!("capture: skipped (no Screen Recording access; capturing could prompt)");
    }

    if require && !missing.is_empty() {
        eprintln!("ci_probe: --require-screen, but this machine lacks {}", missing.join(", "));
        std::process::exit(1);
    }
}
