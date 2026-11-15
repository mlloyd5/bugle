//! The *Open at Login* menu item. Offered only when running from Bugle.app,
//! since `SMAppService` registers the bundle, not a bare executable.
//!
//! The check mark follows what macOS reports (re-read every few seconds, so
//! changes made in System Settings show up): checked only when the item will
//! actually launch, with a "needs approval" title while the user still has to
//! allow it.

use anyhow::Result;
use bugle_platform::login_item::{self, LoginItemStatus};
use std::time::{Duration, Instant};
use tray_icon::menu::CheckMenuItem;

pub const MENU_ID: &str = "login_item";
const APPROVAL_HINT: &str = "Allow Bugle in System Settings → General → Login Items";
const REFRESH_EVERY: Duration = Duration::from_secs(5);

pub struct LoginMenu {
    pub item: CheckMenuItem,
    last_refresh: Instant,
}

impl LoginMenu {
    /// The check item, or `None` outside an app bundle.
    pub fn new() -> Option<Self> {
        if !login_item::in_app_bundle() {
            return None;
        }
        let item = CheckMenuItem::with_id(MENU_ID, "Open at Login", true, false, None);
        let mut menu = Self { item, last_refresh: Instant::now() };
        let status = menu.sync();
        tracing::debug!("open at login: {status:?}");
        Some(menu)
    }

    /// Re-reads the status every few seconds; call from the periodic tick.
    pub fn refresh(&mut self) {
        if self.last_refresh.elapsed() >= REFRESH_EVERY {
            self.sync();
        }
    }

    /// Acts on a click: unregister when enabled, send the user to System
    /// Settings when approval is pending, register otherwise. Returns a hint
    /// to show when the user still has to approve the item.
    pub fn toggle(&mut self) -> Result<Option<String>> {
        let result = match login_item::status() {
            LoginItemStatus::Enabled => login_item::set_enabled(false).map(|_| None),
            LoginItemStatus::RequiresApproval => Ok(Some(())),
            _ => match login_item::set_enabled(true) {
                Ok(LoginItemStatus::RequiresApproval) => Ok(Some(())),
                Ok(_) => Ok(None),
                // Registering an item the user disallowed fails ("operation
                // not permitted") while the status becomes RequiresApproval.
                Err(_) if login_item::status() == LoginItemStatus::RequiresApproval => Ok(Some(())),
                Err(e) => Err(e),
            },
        };
        // The menu flips the mark on click; show what macOS actually has.
        let status = self.sync();
        tracing::info!("open at login: {status:?}");
        match result? {
            Some(()) => {
                login_item::open_settings();
                Ok(Some(APPROVAL_HINT.to_string()))
            }
            None => Ok(None),
        }
    }

    fn sync(&mut self) -> LoginItemStatus {
        self.last_refresh = Instant::now();
        let status = login_item::status();
        self.item.set_checked(status.is_enabled());
        self.item.set_text(if status == LoginItemStatus::RequiresApproval {
            "Open at Login (needs approval)"
        } else {
            "Open at Login"
        });
        status
    }
}
