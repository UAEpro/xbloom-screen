//! The app's platform hooks on the ESP32: backlight, restart, Wi-Fi, storage, addresses.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use xbloom_app::settings::Wifi;
use xbloom_app::{Hotspot, Platform};

use crate::display::Backlight;
use crate::net::Network;
use crate::storage;

/// Hooks for [`xbloom_app::App`].
pub struct EspHooks {
    /// Wi-Fi.
    pub net: Arc<Network>,
    /// Display backlight.
    pub backlight: Arc<Mutex<Backlight>>,
}

impl Platform for EspHooks {
    fn restart(&self) {
        log::info!("restart requested");
        // Let the HTTP response go out first.
        let _ = std::thread::Builder::new().stack_size(4096).spawn(|| {
            std::thread::sleep(Duration::from_millis(500));
            // SAFETY: plain reboot.
            unsafe { esp_idf_svc::sys::esp_restart() };
        });
    }

    fn set_brightness(&self, percent: u8) {
        if let Ok(mut b) = self.backlight.lock() {
            b.set(percent);
        }
    }

    fn wifi_changed(&self, wifi: &Wifi) {
        self.net.set_credentials(wifi);
    }

    fn storage(&self) -> Option<(u64, u64)> {
        storage::usage()
    }

    fn admin_urls(&self) -> Vec<String> {
        self.net.urls()
    }

    fn setup_hotspot(&self) -> Option<Hotspot> {
        self.net.setup_hotspot()
    }
}
