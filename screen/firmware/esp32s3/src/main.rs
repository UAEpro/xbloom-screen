//! xBloom Screen firmware for ESP32-S3 touch displays.
//!
//! The same product as the Raspberry Pi app — the touchscreen UI, the admin web panel, the
//! recipe library, dial presets, PIN lock and screensaver — on a microcontroller board. All of
//! it is the shared crates (`xbloom-core`, `xbloom-app`, `xbloom-ui`); this firmware only adds
//! the hardware: the RGB panel, GT911 touch, NimBLE, Wi-Fi and the flash file system.
//!
//! 🛑 Safety model (unchanged from the shared driver): loading a recipe only arms the machine.
//! A brew starts only from the "Hold to brew" gesture (when remote start is enabled) or the
//! machine's own button.

mod ble;
mod board;
mod display;
mod hooks;
mod http;
mod net;
mod platform;
mod storage;
mod touch;

use std::path::Path;
use std::sync::{Arc, Mutex};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys;
use slint::ComponentHandle;
use xbloom_app::settings::Settings;
use xbloom_app::{App, Data, Event, PlatformInfo, VideoProfile};
use xbloom_core::sim::SimConfig;

use crate::board::BOARD;

/// Give every thread the app spawns (machine controller, screensaver decoder, simulator…) a
/// stack in PSRAM. Internal RAM is kept for Wi-Fi, Bluetooth, DMA and the UI task. This is
/// safe because code and constants run from PSRAM (`CONFIG_SPIRAM_XIP_FROM_PSRAM`), so flash
/// writes don't disable the cache under those stacks.
fn psram_thread_stacks() {
    // SAFETY: plain ESP-IDF configuration calls on the current task.
    unsafe {
        let mut cfg = sys::esp_pthread_get_default_config();
        cfg.stack_alloc_caps = sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT;
        cfg.inherit_cfg = true;
        if let Err(e) = sys::esp!(sys::esp_pthread_set_cfg(&cfg)) {
            log::warn!("pthread: can't move stacks to PSRAM: {e}");
        }
    }
}

/// Apply the POSIX time zone from the settings (`UTC0`, `CET-1CEST,M3.5.0,M10.5.0/3`, …).
fn apply_timezone(tz: &str) {
    std::env::set_var("TZ", if tz.is_empty() { "UTC0" } else { tz });
    xbloom_ui::clock::reload_zone();
}

fn main() {
    sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    log::info!("xBloom Screen {} on {}", env!("CARGO_PKG_VERSION"), BOARD.name);
    psram_thread_stacks();

    let peripherals = Peripherals::take().expect("peripherals");
    let sysloop = EspSystemEventLoop::take().expect("event loop");
    let nvs = EspDefaultNvsPartition::take().expect("nvs");

    // Data partition (settings, recipes, history, media).
    if let Err(e) = storage::mount() {
        log::error!("storage: can't mount the data partition: {e}");
    }
    let data = Path::new(storage::DATA_DIR);
    let settings: Settings =
        xbloom_app::util::read_json(&data.join("settings.json")).ok().flatten().unwrap_or_default();
    apply_timezone(&settings.timezone);

    // Display, backlight, touch, and Slint on top.
    let backlight = display::Backlight::new(peripherals.ledc.timer0, peripherals.ledc.channel0, BOARD.backlight)
        .expect("backlight");
    let backlight = Arc::new(Mutex::new(backlight));
    let panel = display::Display::new(&BOARD).expect("display");
    let touch = touch::Gt911::new(peripherals.i2c0, &BOARD.touch, panel.size()).expect("touch");
    // The screen as the UI sees it: a quarter turn swaps the sides.
    let size = match panel.size() {
        (w, h) if settings.appearance.rotation % 180 == 90 => (h, w),
        wh => wh,
    };
    platform::EspPlatform::install(panel, touch, settings.appearance.rotation).expect("slint platform");
    let ui = xbloom_ui::AppWindow::new().expect("ui");

    // Wi-Fi (or the setup hotspot), then the app.
    let net = net::Network::start(peripherals.modem, sysloop, nvs, &settings.wifi, data, 80).expect("wifi");
    let info = PlatformInfo {
        name: "esp32s3".into(),
        screen: size,
        // Half-resolution video: the JPEG decoder keeps ~10 fps, and the screen scales it up.
        video: VideoProfile { width: size.0 / 2, height: size.1 / 2, fps: 10, quality: 0.7, max_seconds: 30 },
        pin_iterations: 10_000,
        history_max: 200,
        history_points: 160,
        max_upload: 4_000_000,
        version: env!("CARGO_PKG_VERSION").into(),
        can_rotate: true,
    };
    let hooks = hooks::EspHooks { net: Arc::clone(&net), backlight };
    let app = App::start(data, info, Box::new(ble::NimbleTransport::new()), Box::new(hooks), SimConfig::default());
    let weak = Arc::downgrade(&app);
    let rotation = settings.appearance.rotation;
    app.subscribe(move |ev| {
        if let (Event::Changed(Data::Settings), Some(app)) = (ev, weak.upgrade()) {
            let s = app.settings();
            apply_timezone(&s.timezone);
            // A new rotation is applied at start-up.
            if s.appearance.rotation != rotation {
                app.hooks().restart();
            }
        }
    });
    let _server = match http::start(Arc::clone(&app), Arc::clone(&net)) {
        Ok(s) => Some(s),
        Err(e) => {
            log::error!("http: {e}");
            None
        }
    };

    xbloom_ui::bind(&ui, Arc::clone(&app));
    if let Err(e) = ui.run() {
        log::error!("ui: {e}");
    }
    app.shutdown();
    // SAFETY: plain reboot; the UI loop only ends on a fatal error.
    unsafe { sys::esp_restart() };
}
