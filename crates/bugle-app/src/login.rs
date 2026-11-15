//! The *Open at Login* menu item. Offered only when running from Bugle.app,
//! since `SMAppService` registers the bundle, not a bare executable.

use anyhow::Result;
use bugle_platform::login_item::{self, LoginItemStatus};
use tray_icon::menu::CheckMenuItem;

pub const MENU_ID: &str = "login_item";

/// The check item, or `None` outside an app bundle.
pub fn menu_item() -> Option<CheckMenuItem> {
    if !login_item::in_app_bundle() {
        return None;
    }
    let status = login_item::status();
    tracing::debug!("open at login: {status:?}");
    let on = status.is_registered();
    Some(CheckMenuItem::with_id(MENU_ID, "Open at Login", true, on, None))
}

/// Flips the setting and syncs the check mark with the real status. Returns
/// a message when the user still has to approve it in System Settings.
pub fn toggle(item: &CheckMenuItem) -> Result<Option<String>> {
    let want = !login_item::status().is_registered();
    let result = login_item::set_enabled(want);
    // The menu toggles the mark on click; show what macOS actually has.
    item.set_checked(login_item::status().is_registered());
    let status = result?;
    tracing::info!("open at login: {status:?}");
    Ok((status == LoginItemStatus::RequiresApproval)
        .then(|| "Allow Bugle in System Settings → General → Login Items".to_string()))
}
