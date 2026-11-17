//! Quick-access panels (`shim/panel.m`): floating, non-activating panels with
//! a thumbnail, two lines of text, a row of buttons and a close button.
//!
//! A panel is dumb on purpose: it reports clicks and closes. Which buttons
//! it shows, what they do and when it times out belong to the caller.
//! Several panels stack up in the chosen corner, the newest nearest to it;
//! the caller names each by a non-zero id of its own.
//! Every function here is main-thread only and needs a running AppKit loop.

use super::{cstr, path_cstr};
use anyhow::{bail, Result};
use std::ffi::CString;
use std::os::raw::{c_char, c_void};
use std::path::Path;

type PanelCallback = unsafe extern "C" fn(ctx: *mut c_void, choice: i32);
type PanelFree = unsafe extern "C" fn(ctx: *mut c_void);

extern "C" {
    fn bugle_panel_show(
        id: u64,
        png: *const u8,
        len: usize,
        title: *const c_char,
        subtitle: *const c_char,
        labels: *const *const c_char,
        n_labels: i32,
        corner: i32,
        cb: PanelCallback,
        ctx: *mut c_void,
        free_ctx: PanelFree,
    ) -> i32;
    fn bugle_panel_set_image(id: u64, png: *const u8, len: usize);
    fn bugle_panel_dismiss(id: u64, animated: i32);
    fn bugle_panel_dismiss_all(animated: i32);
    fn bugle_panel_hide_all();
    fn bugle_panel_show_all();
    fn bugle_panel_count() -> i32;
    fn bugle_panel_hovered() -> i32;
    fn bugle_reveal_path(path: *const c_char);
    fn bugle_open_with(path: *const c_char, bundle_id: *const c_char, err: *mut c_char, err_len: usize) -> i32;
}

/// What the user clicked on the panel. The panel has already closed itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelChoice {
    /// One of the `buttons` passed to [`panel_show`], by index.
    Button(usize),
    /// The close (×) button.
    Close,
    /// The thumbnail.
    Thumbnail,
}

impl PanelChoice {
    fn from_raw(choice: i32) -> Option<Self> {
        match choice {
            -1 => Some(Self::Close),
            -2 => Some(Self::Thumbnail),
            n if n >= 0 => Some(Self::Button(n as usize)),
            _ => None,
        }
    }
}

/// Screen corner for the panel, as `[quick_access] position` names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Corner {
    #[default]
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
}

type Handler = Box<dyn FnMut(PanelChoice)>;

/// Show panel `id` (non-zero) on the screen under the pointer, in the corner,
/// with the panels already up moving one place away from it (all of them on
/// that screen and corner). A panel already up with the same `id` is
/// replaced. While the stack is hidden by [`panel_hide_all`] the new panel
/// joins it off screen and comes back with [`panel_show_all`]. `on_choice` runs
/// on the main thread for at most one click; it is dropped when the panel
/// closes for any reason.
pub fn panel_show(
    id: u64,
    png: Option<&[u8]>,
    title: &str,
    subtitle: &str,
    buttons: &[&str],
    corner: Corner,
    on_choice: impl FnMut(PanelChoice) + 'static,
) -> Result<()> {
    unsafe extern "C" fn call(ctx: *mut c_void, choice: i32) {
        let h = unsafe { &mut *(ctx as *mut Handler) };
        if let Some(c) = PanelChoice::from_raw(choice) {
            h(c);
        }
    }
    unsafe extern "C" fn free(ctx: *mut c_void) {
        drop(unsafe { Box::from_raw(ctx as *mut Handler) });
    }
    let (t, st) = (cstr(title)?, cstr(subtitle)?);
    let labels: Vec<CString> = buttons.iter().map(|b| cstr(b)).collect::<Result<_>>()?;
    let ptrs: Vec<*const c_char> = labels.iter().map(|c| c.as_ptr()).collect();
    let (ptr, len) = png.map(|p| (p.as_ptr(), p.len())).unwrap_or((std::ptr::null(), 0));
    let corner = match corner {
        Corner::TopRight => 0,
        Corner::TopLeft => 1,
        Corner::BottomRight => 2,
        Corner::BottomLeft => 3,
    };
    if id == 0 {
        bail!("a quick-access panel needs a non-zero id");
    }
    let handler: Box<Handler> = Box::new(Box::new(on_choice));
    let ctx = Box::into_raw(handler) as *mut c_void;
    // On success the shim owns `ctx` and frees it through `free`.
    let ok = unsafe {
        bugle_panel_show(id, ptr, len, t.as_ptr(), st.as_ptr(), ptrs.as_ptr(), ptrs.len() as i32, corner, call, ctx, free)
    };
    if ok != 1 {
        // The shim did not take the context.
        drop(unsafe { Box::from_raw(ctx as *mut Handler) });
        bail!("could not show the quick-access panel");
    }
    Ok(())
}

/// Replace panel `id`'s thumbnail, e.g. with a sharper one rendered off the
/// main thread. No-op when that panel is gone.
pub fn panel_set_image(id: u64, png: &[u8]) {
    unsafe { bugle_panel_set_image(id, png.as_ptr(), png.len()) }
}

/// Close panel `id` (fading out when `animated`) without reporting a click;
/// the panels behind it move up into its place.
pub fn panel_dismiss(id: u64, animated: bool) {
    unsafe { bugle_panel_dismiss(id, animated as i32) }
}

/// Close every panel (fading out when `animated`; otherwise at once,
/// including panels still fading).
pub fn panel_dismiss_all(animated: bool) {
    unsafe { bugle_panel_dismiss_all(animated as i32) }
}

/// Take every panel off screen at once, including ones still fading out, so
/// the next screen capture cannot contain them. They stay up as far as the
/// caller is concerned, until [`panel_show_all`] brings them back. Hidden panels never count as hovered.
pub fn panel_hide_all() {
    unsafe { bugle_panel_hide_all() }
}

/// Bring back the panels hidden by [`panel_hide_all`]. No-op otherwise.
pub fn panel_show_all() {
    unsafe { bugle_panel_show_all() }
}

/// How many panels are up, hidden ones included.
pub fn panel_count() -> usize {
    usize::try_from(unsafe { bugle_panel_count() }).unwrap_or(0)
}

/// Whether the pointer is over any panel on screen.
pub fn panel_hovered() -> bool {
    unsafe { bugle_panel_hovered() == 1 }
}

/// Select `path` in a Finder window.
pub fn reveal_path(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("{} no longer exists", path.display());
    }
    let c = path_cstr(path)?;
    unsafe { bugle_reveal_path(c.as_ptr()) }
    Ok(())
}

/// How [`open_with`] opened the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened {
    /// With the requested application (or the default one when none was asked for).
    AsRequested,
    /// With the default application, because the requested bundle id is not installed.
    FellBack,
}

/// Open `path` with the application whose bundle id is `bundle_id` (`None`:
/// the default application) and wait until the workspace reports back.
/// Blocks for as long as the application takes to accept the file, so call
/// it off the main thread.
pub fn open_with(path: &Path, bundle_id: Option<&str>) -> Result<Opened> {
    if !path.exists() {
        bail!("{} no longer exists", path.display());
    }
    let p = path_cstr(path)?;
    let b = bundle_id.map(cstr).transpose()?;
    let mut err = vec![0u8; 512];
    let r = unsafe {
        bugle_open_with(
            p.as_ptr(),
            b.as_ref().map(|c| c.as_ptr()).unwrap_or(std::ptr::null()),
            err.as_mut_ptr() as *mut c_char,
            err.len(),
        )
    };
    match r {
        1 => Ok(Opened::AsRequested),
        2 => Ok(Opened::FellBack),
        _ => {
            let msg = std::ffi::CStr::from_bytes_until_nul(&err).map(|c| c.to_string_lossy().into_owned()).unwrap_or_default();
            bail!("could not open {}: {}", path.display(), if msg.is_empty() { "unknown error" } else { &msg })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choice_codes() {
        assert_eq!(PanelChoice::from_raw(0), Some(PanelChoice::Button(0)));
        assert_eq!(PanelChoice::from_raw(4), Some(PanelChoice::Button(4)));
        assert_eq!(PanelChoice::from_raw(-1), Some(PanelChoice::Close));
        assert_eq!(PanelChoice::from_raw(-2), Some(PanelChoice::Thumbnail));
        assert_eq!(PanelChoice::from_raw(-7), None);
    }
}
