//! Window picking for *Capture Window*: a see-through overlay highlights the
//! window under the pointer; a click picks it, Esc / right click / timeout
//! cancels. The overlay lives in the platform shim and only reports pointer
//! positions; [`window_at`] decides which window that is.

use crate::{AppEvent, Proxy, UiRequest};
use anyhow::{anyhow, Result};
use bugle_core::{Point, Rect, WindowInfo};
use bugle_platform::PickEvent;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

/// How long the overlay waits for a choice before closing itself.
pub const PICK_TIMEOUT: Duration = Duration::from_secs(30);

/// Smallest side, in points, of a window worth picking. Matches what
/// `bugle_platform::app_windows` (and so window capture by id) accepts.
pub const MIN_SIDE: f64 = 50.0;

/// What the hit test ignores.
#[derive(Debug, Clone, Default)]
pub struct PickFilter {
    /// Bugle's own process: the overlay, toasts and pins are never picked.
    pub own_pid: i32,
    /// Display frames; windows that touch none of them are off screen.
    /// Empty means "don't check".
    pub displays: Vec<Rect>,
}

impl PickFilter {
    pub fn current() -> Self {
        let displays = bugle_platform::displays().map(|d| d.into_iter().map(|d| d.frame).collect()).unwrap_or_default();
        Self { own_pid: std::process::id() as i32, displays }
    }

    /// An ordinary application window that is on screen, big enough to
    /// matter and not Bugle's own.
    pub fn accepts(&self, w: &WindowInfo) -> bool {
        w.layer == 0
            && w.on_screen
            && !w.owner.is_empty()
            && w.pid != self.own_pid
            && w.frame.is_valid()
            && w.frame.w >= MIN_SIDE
            && w.frame.h >= MIN_SIDE
            && (self.displays.is_empty() || self.displays.iter().any(|d| d.intersection(&w.frame).is_some()))
    }
}

/// The window under `p` (global points, top-left origin). `windows` is in
/// window-server z-order, front-most first, as `bugle_platform::windows`
/// returns it; the first window the filter accepts that contains `p` wins.
pub fn window_at<'a>(windows: &'a [WindowInfo], p: Point, filter: &PickFilter) -> Option<&'a WindowInfo> {
    windows.iter().find(|w| w.frame.contains(p) && filter.accepts(w))
}

/// Text drawn on the highlighted window.
pub fn label(w: &WindowInfo) -> String {
    let title = w.title.trim();
    let name = if title.is_empty() || title == w.owner { w.owner.clone() } else { format!("{} — {title}", w.owner) };
    format!("{name}  {:.0} × {:.0}", w.frame.w, w.frame.h)
}

/// Main thread: show the overlay and answer `reply` with the picked window,
/// or `None` when cancelled or clicked outside every window. Returns `false`
/// (dropping `reply`, which the waiting worker sees as an error) when an
/// overlay is already up.
pub fn begin(timeout: Duration, reply: Sender<Option<WindowInfo>>) -> bool {
    let filter = PickFilter::current();
    // Snapshot before the overlay exists; the filter drops Bugle's own
    // windows anyway.
    let windows: Vec<WindowInfo> = match bugle_platform::windows(true) {
        Ok(w) => w.into_iter().filter(|w| filter.accepts(w)).collect(),
        Err(e) => {
            tracing::warn!("window picker: {e:#}");
            return false;
        }
    };
    // Outer `None` until the first move, so the first position is always drawn.
    let mut current: Option<Option<u32>> = None;
    let mut reply = Some(reply);
    bugle_platform::window_pick_begin(timeout, move |ev| match ev {
        PickEvent::Moved(p) => {
            let hit = window_at(&windows, p, &filter);
            let id = hit.map(|w| w.id);
            if current != Some(id) {
                current = Some(id);
                tracing::debug!("window picker: {:?} at {},{}", hit.map(label), p.x, p.y);
                bugle_platform::window_pick_highlight(hit.map(|w| w.frame), &hit.map(label).unwrap_or_default());
            }
        }
        PickEvent::Clicked(p) => {
            if let Some(tx) = reply.take() {
                let _ = tx.send(window_at(&windows, p, &filter).cloned());
            }
        }
        PickEvent::Cancelled => {
            if let Some(tx) = reply.take() {
                let _ = tx.send(None);
            }
        }
    })
}

/// Worker thread: ask the main thread for a window and wait for the answer.
/// `Ok(None)` means the user cancelled.
pub fn pick(proxy: &Proxy, timeout: Duration) -> Result<Option<WindowInfo>> {
    let (tx, rx) = mpsc::channel();
    proxy.send(AppEvent::Ui(UiRequest::PickWindow { timeout, reply: tx }));
    // The overlay closes itself at `timeout`; the slack covers the hop back.
    match rx.recv_timeout(timeout + Duration::from_secs(5)) {
        Ok(w) => Ok(w),
        Err(RecvTimeoutError::Timeout) => Err(anyhow!("window picker did not answer")),
        Err(RecvTimeoutError::Disconnected) => Err(anyhow!("window picker unavailable (another overlay is open)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: u32, pid: i32, x: f64, y: f64, w: f64, h: f64) -> WindowInfo {
        WindowInfo {
            id,
            pid,
            owner: format!("App{pid}"),
            title: format!("Window {id}"),
            frame: Rect::new(x, y, w, h),
            layer: 0,
            on_screen: true,
        }
    }

    fn filter() -> PickFilter {
        PickFilter { own_pid: 1, displays: vec![Rect::new(0.0, 0.0, 1440.0, 900.0), Rect::new(1440.0, 0.0, 1920.0, 1080.0)] }
    }

    fn at(ws: &[WindowInfo], x: f64, y: f64) -> Option<u32> {
        window_at(ws, Point { x, y }, &filter()).map(|w| w.id)
    }

    #[test]
    fn front_most_window_containing_the_point_wins() {
        let ws = [win(10, 2, 100.0, 100.0, 400.0, 300.0), win(20, 3, 0.0, 0.0, 1000.0, 800.0)];
        assert_eq!(at(&ws, 150.0, 150.0), Some(10));
        assert_eq!(at(&ws, 50.0, 50.0), Some(20));
        assert_eq!(at(&ws, 1200.0, 50.0), None);
    }

    #[test]
    fn edges_are_half_open() {
        let ws = [win(10, 2, 100.0, 100.0, 400.0, 300.0)];
        assert_eq!(at(&ws, 100.0, 100.0), Some(10));
        assert_eq!(at(&ws, 499.9, 399.9), Some(10));
        assert_eq!(at(&ws, 500.0, 200.0), None);
        assert_eq!(at(&ws, 200.0, 400.0), None);
    }

    #[test]
    fn skips_own_overlay_and_non_application_windows() {
        let mut overlay = win(1, 1, 0.0, 0.0, 1440.0, 900.0); // Bugle's own pid
        overlay.layer = 1000;
        let own_toast = win(2, 1, 100.0, 100.0, 300.0, 80.0);
        let mut menu_bar = win(3, 4, 0.0, 0.0, 1440.0, 24.0);
        menu_bar.layer = 25;
        let mut hidden = win(4, 5, 0.0, 0.0, 800.0, 600.0);
        hidden.on_screen = false;
        let mut ownerless = win(5, 6, 0.0, 0.0, 800.0, 600.0);
        ownerless.owner.clear();
        let target = win(6, 7, 0.0, 0.0, 800.0, 600.0);
        let ws = [overlay, own_toast, menu_bar, hidden, ownerless, target];
        assert_eq!(at(&ws, 150.0, 120.0), Some(6));
        assert_eq!(at(&ws, 10.0, 10.0), Some(6));
    }

    #[test]
    fn skips_tiny_and_off_screen_windows() {
        let tiny = win(1, 2, 100.0, 100.0, 40.0, 400.0);
        let sliver = win(2, 2, 100.0, 100.0, 400.0, 10.0);
        let off = win(3, 2, -5000.0, -5000.0, 6000.0, 5100.0); // reaches on screen
        let far = win(4, 2, 9000.0, 9000.0, 400.0, 400.0);
        let bad = win(5, 2, f64::NAN, 0.0, 400.0, 400.0);
        let target = win(6, 2, 0.0, 0.0, 800.0, 600.0);
        let f = filter();
        assert!(!f.accepts(&tiny) && !f.accepts(&sliver) && !f.accepts(&far) && !f.accepts(&bad));
        assert!(f.accepts(&off), "a window partly on screen is pickable");
        let ws = [tiny, sliver, far.clone(), bad, target];
        assert_eq!(at(&ws, 110.0, 105.0), Some(6));
        assert_eq!(window_at(&[far], Point { x: 9100.0, y: 9100.0 }, &f).map(|w| w.id), None);
    }

    #[test]
    fn second_display_and_negative_coordinates() {
        let mut f = filter();
        f.displays.push(Rect::new(-1280.0, 0.0, 1280.0, 800.0));
        let left = win(1, 2, -1000.0, 100.0, 600.0, 400.0);
        let right = win(2, 2, 1600.0, 100.0, 600.0, 400.0);
        let ws = [left, right];
        assert_eq!(window_at(&ws, Point { x: -900.0, y: 200.0 }, &f).map(|w| w.id), Some(1));
        assert_eq!(window_at(&ws, Point { x: 1700.0, y: 200.0 }, &f).map(|w| w.id), Some(2));
    }

    #[test]
    fn empty_display_list_disables_the_off_screen_check() {
        let f = PickFilter { own_pid: 1, displays: vec![] };
        let far = win(4, 2, 9000.0, 9000.0, 400.0, 400.0);
        assert!(f.accepts(&far));
    }

    #[test]
    fn labels() {
        let mut w = win(1, 2, 0.0, 0.0, 800.0, 600.0);
        assert_eq!(label(&w), "App2 — Window 1  800 × 600");
        w.title.clear();
        assert_eq!(label(&w), "App2  800 × 600");
    }
}
