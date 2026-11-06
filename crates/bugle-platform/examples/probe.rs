//! Diagnostic: `cargo run -p bugle-platform --example probe`.
//! Reports permissions, displays, windows, and times one capture + OCR.
use bugle_platform as p;

fn main() {
    println!("screen access: {}", p::screen_access_preflight());
    if !p::screen_access_preflight() {
        println!("requesting screen access -> {}", p::screen_access_request());
    }
    println!("accessibility: {}", p::accessibility_trusted(false));
    for d in p::displays().unwrap() {
        println!("display {:?}", d);
    }
    let wins = p::app_windows().unwrap();
    for w in wins.iter().take(8) {
        println!("window {} {} '{}' {}", w.id, w.owner, w.title, w.frame);
    }
    let t = std::time::Instant::now();
    match p::capture_display(None, false) {
        Ok(img) => {
            println!("display capture {}x{} in {:?}", img.width(), img.height(), t.elapsed());
            let t = std::time::Instant::now();
            let lines = p::ocr(&img, &p::OcrParams { languages: &[], accurate: true, language_correction: true }).unwrap();
            println!("ocr: {} lines in {:?}", lines.len(), t.elapsed());
            for l in lines.iter().take(5) {
                println!("  {:?} {}", l.text, l.bbox);
            }
        }
        Err(e) => println!("display capture error: {e}"),
    }
    if let Some(w) = wins.first() {
        match p::capture_window(w.id, true, false) {
            Ok(img) => println!("window capture (shadow) {}x{} frame {}", img.width(), img.height(), w.frame),
            Err(e) => println!("window error: {e}"),
        }
    }
}
