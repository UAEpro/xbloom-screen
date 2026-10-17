//! Render every screen of the UI to PNG, in light and dark, driven by the real app on the
//! built-in simulator (a genuine simulated brew, not mock data).
//!
//! ```sh
//! cargo run -p xbloom-ui --example screenshots --features screenshot -- docs/img/screen
//! # another panel size (optionally with an interface scale), light theme only:
//! cargo run -p xbloom-ui --example screenshots --features screenshot -- out 480x320 quick
//! cargo run -p xbloom-ui --example screenshots --features screenshot -- out 1920x1080@1.5 quick
//! ```

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model};
use xbloom_app::settings::{LockMode, ScreensaverMode, Settings};
use xbloom_app::{App, Link, PlatformInfo, VideoProfile};
use xbloom_core::sim::{SimConfig, SimTransport};
use xbloom_ui::screenshot::Shooter;
use xbloom_ui::{AppState, AppWindow, Overlay, Page};

/// Flipped near the end to show an ESP32 on its own setup hotspot.
static HOTSPOT: AtomicBool = AtomicBool::new(false);

struct Hooks;
impl xbloom_app::Platform for Hooks {
    fn admin_urls(&self) -> Vec<String> {
        if HOTSPOT.load(Ordering::Relaxed) {
            vec!["http://192.168.4.1".into()]
        } else {
            vec!["http://192.168.1.42:8080".into()]
        }
    }
    fn storage(&self) -> Option<(u64, u64)> {
        Some((11_800_000_000, 14_900_000_000))
    }
    fn setup_hotspot(&self) -> Option<xbloom_app::Hotspot> {
        HOTSPOT
            .load(Ordering::Relaxed)
            .then(|| xbloom_app::Hotspot { ssid: "xBloom-Screen-1A2B".into(), password: "k7mx2pq9tz".into() })
    }
}

/// A synthetic "pour" clip so the video screensaver has something to show.
fn demo_video(dir: &Path) {
    let (w, h) = (400u16, 240u16);
    let frames: Vec<Vec<u8>> = (0..12)
        .map(|f| {
            let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
            for y in 0..h as i32 {
                for x in 0..w as i32 {
                    let dx = (x - 200 - f * 3) as f32;
                    let dy = (y - 120) as f32;
                    let d = (dx * dx + dy * dy).sqrt();
                    let glow = (1.0 - (d / 260.0)).clamp(0.0, 1.0);
                    rgb.extend_from_slice(&[
                        (40.0 + 170.0 * glow) as u8,
                        (26.0 + 90.0 * glow) as u8,
                        (18.0 + 40.0 * glow) as u8,
                    ]);
                }
            }
            let mut out = Vec::new();
            jpeg_encoder::Encoder::new(&mut out, 80).encode(&rgb, w, h, jpeg_encoder::ColorType::Rgb).unwrap();
            out
        })
        .collect();
    let mut file = std::fs::File::create(dir.join("media/morning-pour.xbv")).unwrap();
    xbloom_app::media::write_xbv(&mut file, w, h, 1200, &frames).unwrap();
}

fn wait(shot: &Shooter, what: &str, timeout: Duration, f: impl Fn() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < timeout, "timed out waiting for {what}");
        shot.pump(Duration::from_millis(50));
    }
}

fn main() {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "screenshots".into()));
    let size = std::env::args().nth(2).unwrap_or_else(|| "800x480".into());
    let (size, scale) = size.split_once('@').map_or((size.as_str(), 1.0f32), |(s, f)| (s, f.parse().unwrap_or(1.0)));
    let (w, h) =
        size.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))).unwrap_or((800u32, 480u32));
    // "quick": light theme only, no video screensaver.
    let quick = std::env::args().nth(3).as_deref() == Some("quick");
    std::fs::create_dir_all(&out).unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(data.path().join("media")).unwrap();
    demo_video(data.path());

    let mut s = Settings { setup_complete: true, ..Settings::default() };
    s.machine.simulator = true;
    s.screensaver.video = Some("morning-pour.xbv".into());
    s.appearance.ui_scale = (scale * 100.0).round() as u16;
    xbloom_app::util::write_json(&data.path().join("settings.json"), &s).unwrap();
    let platform = PlatformInfo {
        name: "raspberry-pi".into(),
        screen: (w, h),
        video: VideoProfile { width: w, height: h, fps: 24, quality: 0.8, max_seconds: 60 },
        pin_iterations: 1000,
        history_max: 100,
        history_points: 300,
        max_upload: 200_000_000,
        version: env!("CARGO_PKG_VERSION").into(),
        can_rotate: true,
    };
    let sim = SimConfig { speed: 12.0, grind_s: 18.0, cup_lift_s: 30.0, ..SimConfig::default() };
    let app: Arc<App> =
        App::start(data.path(), platform, Box::new(SimTransport::new(SimConfig::default())), Box::new(Hooks), sim);
    app.setup_admin_pin("2580").unwrap();

    let shot = Shooter::install(w, h);
    let ui = AppWindow::new().unwrap();
    xbloom_ui::bind(&ui, Arc::clone(&app));
    let g = ui.global::<AppState>();
    let snap = |name: &str| {
        shot.capture(&out.join(format!("{name}.png"))).unwrap();
        println!("  {name}.png");
    };
    wait(&shot, "connected", Duration::from_secs(10), || app.machine_status().link == Link::Connected);
    shot.pump(Duration::from_millis(400));

    for dark in if quick { &[false][..] } else { &[false, true][..] } {
        let dark = *dark;
        let sfx = if dark { "dark" } else { "light" };
        app.update_settings(|s| {
            s.appearance.theme =
                if dark { xbloom_app::settings::Theme::Dark } else { xbloom_app::settings::Theme::Light }
        });
        shot.pump(Duration::from_millis(300));

        g.set_page(Page::Home);
        shot.pump(Duration::from_millis(300));
        snap(&format!("home-{sfx}"));

        let first = g.get_recipes().iter().next().unwrap().id.to_string();
        g.invoke_open_recipe(first.into());
        shot.pump(Duration::from_millis(300));
        snap(&format!("recipe-{sfx}"));

        // A real simulated brew, started the way a person would: the hold-to-brew gesture.
        g.invoke_start_brew(true);
        wait(&shot, "pouring", Duration::from_secs(60), || {
            let b = g.get_brew();
            b.stage.as_str() == "brewing" && b.progress > 0.45
        });
        shot.pump(Duration::from_millis(300));
        snap(&format!("brewing-{sfx}"));
        if !dark {
            g.set_overlay(Overlay::CancelBrew);
            shot.pump(Duration::from_millis(200));
            snap("stop-dialog");
            g.set_overlay(Overlay::None);
        }
        wait(&shot, "brew finished", Duration::from_secs(120), || g.get_brew().finished);
        shot.pump(Duration::from_millis(500));
        snap(&format!("brew-done-{sfx}"));
        g.invoke_brew_dismiss();

        g.invoke_navigate(Page::History);
        shot.pump(Duration::from_millis(300));
        snap(&format!("history-{sfx}"));

        // Assign the dial presets.
        let ids: Vec<String> = g.get_recipes().iter().map(|r| r.id.to_string()).collect();
        for (i, id) in ids.iter().take(3).enumerate() {
            g.invoke_assign_slot(i as i32, id.as_str().into());
        }
        g.invoke_navigate(Page::Presets);
        shot.pump(Duration::from_millis(300));
        snap(&format!("presets-{sfx}"));
        if !dark {
            g.set_picking_slot(1);
            g.set_overlay(Overlay::PickRecipe);
            shot.pump(Duration::from_millis(200));
            snap("pick-recipe");
            g.set_overlay(Overlay::None);
        }

        // Settings are behind the admin PIN.
        g.invoke_navigate(Page::Settings);
        for k in ["2", "5", "8"] {
            g.invoke_pin_key(k.into());
        }
        shot.pump(Duration::from_millis(200));
        snap(&format!("pin-{sfx}"));
        g.invoke_pin_key("0".into());
        g.invoke_pin_key("ok".into());
        shot.pump(Duration::from_millis(300));
        for (tab, name) in [(0, "machine"), (1, "display"), (2, "screensaver"), (3, "security"), (4, "admin")] {
            g.set_settings_tab(tab);
            shot.pump(Duration::from_millis(250));
            snap(&format!("settings-{name}-{sfx}"));
        }
        g.invoke_navigate(Page::Home);
        shot.pump(Duration::from_millis(200));

        g.set_overlay(Overlay::Machine);
        shot.pump(Duration::from_millis(200));
        snap(&format!("machine-sheet-{sfx}"));
        g.set_overlay(Overlay::None);

        // Screen lock.
        app.update_settings(|s| s.security.lock_mode = LockMode::Screen);
        shot.pump(Duration::from_millis(200));
        xbloom_ui::bind::force_lock();
        shot.pump(Duration::from_millis(300));
        snap(&format!("lock-{sfx}"));
        for k in ["2", "5", "8", "0", "ok"] {
            g.invoke_pin_key(k.into());
        }
        app.update_settings(|s| s.security.lock_mode = LockMode::Off);

        // Screensavers.
        app.update_settings(|s| s.screensaver.mode = ScreensaverMode::Clock);
        shot.pump(Duration::from_millis(200));
        xbloom_ui::bind::force_screensaver();
        shot.pump(Duration::from_millis(400));
        snap(&format!("screensaver-clock-{sfx}"));
        xbloom_ui::bind::force_wake();
        if !dark && !quick {
            app.update_settings(|s| s.screensaver.mode = ScreensaverMode::Video);
            shot.pump(Duration::from_millis(200));
            xbloom_ui::bind::force_screensaver();
            wait(&shot, "a video frame", Duration::from_secs(5), || g.get_saver_has_frame());
            shot.pump(Duration::from_millis(200));
            snap("screensaver-video");
            xbloom_ui::bind::force_wake();
        }
    }

    // An ESP32 without Wi-Fi: the admin tab shows how to join its hotspot (light theme).
    HOTSPOT.store(true, Ordering::Relaxed);
    app.update_settings(|s| s.appearance.theme = xbloom_app::settings::Theme::Light);
    g.invoke_navigate(Page::Settings);
    for k in ["2", "5", "8", "0", "ok"] {
        g.invoke_pin_key(k.into());
    }
    g.set_settings_tab(4);
    shot.pump(Duration::from_millis(300));
    snap("settings-admin-hotspot");
    g.invoke_navigate(Page::Home);
    HOTSPOT.store(false, Ordering::Relaxed);

    // First-run setup screens (pure UI state).
    app.update_settings(|s| s.appearance.theme = xbloom_app::settings::Theme::Light);
    shot.pump(Duration::from_millis(200));
    g.set_needs_setup(true);
    g.set_setup_step(0);
    shot.pump(Duration::from_millis(300));
    snap("setup-welcome");
    g.set_setup_step(1);
    g.set_pin_title("Create an admin PIN".into());
    g.set_pin_subtitle("4 to 8 digits. It protects the settings and the admin panel.".into());
    shot.pump(Duration::from_millis(300));
    snap("setup-pin");
    g.set_setup_step(3);
    shot.pump(Duration::from_millis(300));
    snap("setup-done");
    app.shutdown();
}
