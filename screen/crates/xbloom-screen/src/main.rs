//! `xbloom-screen` — the xBloom touchscreen controller for Raspberry Pi and Linux.
//!
//! Runs full-screen on DRM/KMS (Raspberry Pi OS Lite, no desktop) or in a window on a desktop,
//! talks to the machine over Bluetooth LE (BlueZ), and serves the admin panel on the LAN.
//!
//! ```text
//! xbloom-screen [--data DIR] [--port 8080] [--demo] [--sim-speed 1.0]
//! ```

mod ble;
mod http;
mod platform;
mod rotation;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use slint::ComponentHandle;
use xbloom_app::settings::Settings;
use xbloom_app::{App, Data, Event, PlatformInfo, VideoProfile};
use xbloom_core::driver::Transport;
use xbloom_core::sim::{SimConfig, SimTransport};

const HELP: &str = "\
xbloom-screen — touchscreen controller for the xBloom Studio coffee machine

USAGE:
    xbloom-screen [OPTIONS]

OPTIONS:
    --data DIR         Where settings, recipes, history and media live
                       [default: $STATE_DIRECTORY, else ~/.local/share/xbloom-screen]
    --port PORT        Admin panel port [default: 8080]
    --bind ADDR        Admin panel address [default: 0.0.0.0]
    --demo             Start in demo mode (built-in simulated machine)
    --sim-speed X      Simulated time per real second in demo mode [default: 1]
    --no-http          Don't start the admin panel
    -V, --version      Print the version
    -h, --help         Print this help

ENVIRONMENT:
    SLINT_BACKEND      linuxkms-software (kiosk, default without a desktop) or winit
    SLINT_KMS_ROTATION 0, 90, 180 or 270: overrides the rotation set on the screen
    RUST_LOG           Log filter, e.g. info or xbloom_core=debug (BLE frame trace)
";

struct Args {
    data: Option<PathBuf>,
    port: u16,
    bind: IpAddr,
    demo: bool,
    sim_speed: f32,
    http: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        data: None,
        port: 8080,
        bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        demo: false,
        sim_speed: 1.0,
        http: true,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--data" => a.data = Some(PathBuf::from(value("--data")?)),
            "--port" => a.port = value("--port")?.parse().map_err(|_| "--port must be a number")?,
            "--bind" => a.bind = value("--bind")?.parse().map_err(|_| "--bind must be an IP address")?,
            "--demo" => a.demo = true,
            "--sim-speed" => a.sim_speed = value("--sim-speed")?.parse().map_err(|_| "--sim-speed must be a number")?,
            "--no-http" => a.http = false,
            "-V" | "--version" => {
                println!("xbloom-screen {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown option {other} (see --help)")),
        }
    }
    Ok(a)
}

fn default_data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("STATE_DIRECTORY") {
        // systemd's StateDirectory= (may list several, colon-separated).
        let s = d.to_string_lossy().split(':').next().unwrap_or_default().to_string();
        return PathBuf::from(s);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".local/share"));
    base.join("xbloom-screen")
}

/// The screensaver video size for a `w` × `h` screen: at most 1280 × 800 (either way up),
/// same shape, even sides.
fn video_size(w: u32, h: u32) -> (u32, u32) {
    let (long, short) = (w.max(h) as f32, w.min(h) as f32);
    let k = (1280.0 / long).min(800.0 / short).min(1.0);
    let even = |v: u32| ((v as f32 * k) as u32) & !1;
    (even(w), even(h))
}

fn platform_name() -> String {
    let model = std::fs::read_to_string("/proc/device-tree/model").unwrap_or_default();
    if model.contains("Raspberry Pi") {
        "raspberry-pi".into()
    } else {
        "linux".into()
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    let data = args.data.clone().unwrap_or_else(default_data_dir);
    if let Err(e) = std::fs::create_dir_all(&data) {
        eprintln!("error: can't create the data directory {}: {e}", data.display());
        std::process::exit(1);
    }
    log::info!("xbloom-screen {} — data in {}", env!("CARGO_PKG_VERSION"), data.display());

    // Rotation is for a kiosk on KMS; a desktop rotates its own windows. SLINT_KMS_ROTATION,
    // if set, wins over the setting.
    let settings: Settings =
        xbloom_app::util::read_json(&data.join("settings.json")).ok().flatten().unwrap_or_default();
    let kiosk = std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none();
    let rotation_env: Option<u16> = std::env::var("SLINT_KMS_ROTATION").ok().and_then(|v| v.parse().ok());
    let rotation = match rotation_env {
        Some(r) => r,
        None if kiosk => settings.appearance.rotation,
        None => 0,
    };
    if rotation_env.is_none() && rotation != 0 {
        std::env::set_var("SLINT_KMS_ROTATION", rotation.to_string());
    }

    // The software renderer draws exactly what the ESP32 draws; SLINT_BACKEND can override.
    let mut backend = slint::BackendSelector::new();
    if std::env::var_os("SLINT_BACKEND").is_none() {
        backend = backend.renderer_name("software".into());
    }
    #[cfg(all(target_os = "linux", feature = "backend-linuxkms"))]
    {
        backend = backend.with_libinput_event_hook(rotation::libinput_hook(rotation));
    }
    if let Err(e) = backend.select() {
        log::warn!("display backend: {e}; using Slint's default");
    }
    let ui = match xbloom_ui::AppWindow::new() {
        Ok(ui) => ui,
        Err(e) => {
            eprintln!("error: can't open the display: {e}\n(on a Pi without a desktop, run with SLINT_BACKEND=linuxkms-software and access to /dev/dri + /dev/input)");
            std::process::exit(1);
        }
    };
    let size = ui.window().size();
    let (w, h) = if size.width > 0 { (size.width, size.height) } else { (800, 480) };

    if args.demo {
        let mut s: xbloom_app::settings::Settings =
            xbloom_app::util::read_json(&data.join("settings.json")).ok().flatten().unwrap_or_default();
        s.machine.simulator = true;
        let _ = xbloom_app::util::write_json(&data.join("settings.json"), &s);
    }

    let transport: Box<dyn Transport> = match ble::BtleTransport::new() {
        Ok(t) => Box::new(t),
        Err(e) => {
            log::error!("bluetooth unavailable ({e}); only demo mode will work");
            Box::new(SimTransport::new(SimConfig::default()))
        }
    };
    let (vw, vh) = video_size(w, h);
    let info = PlatformInfo {
        name: platform_name(),
        screen: (w, h),
        video: VideoProfile { width: vw, height: vh, fps: 24, quality: 0.8, max_seconds: 60 },
        pin_iterations: 60_000,
        history_max: 500,
        history_points: 400,
        max_upload: 300_000_000,
        version: env!("CARGO_PKG_VERSION").into(),
        can_rotate: kiosk && rotation_env.is_none(),
    };
    let sim = SimConfig { speed: args.sim_speed.max(0.1), ..SimConfig::default() };
    let app = App::start(&data, info, transport, Box::new(platform::LinuxPlatform::new(&data, args.port)), sim);

    // A new rotation takes a restart (systemd brings the app straight back).
    if app.platform.can_rotate {
        let weak = Arc::downgrade(&app);
        let restarting = AtomicBool::new(false);
        app.subscribe(move |ev| {
            if let (Event::Changed(Data::Settings), Some(app)) = (ev, weak.upgrade()) {
                if app.settings().appearance.rotation != rotation && !restarting.swap(true, Ordering::AcqRel) {
                    log::info!("rotation changed to {}°: restarting", app.settings().appearance.rotation);
                    app.hooks().restart();
                }
            }
        });
    }

    if args.http {
        if let Err(e) = http::start(Arc::clone(&app), SocketAddr::new(args.bind, args.port), 4) {
            log::error!("admin panel: can't listen on port {}: {e}", args.port);
        }
    }

    xbloom_ui::bind(&ui, Arc::clone(&app));
    if let Err(e) = ui.run() {
        log::error!("ui: {e}");
    }
    app.shutdown();
}

#[cfg(test)]
mod tests {
    use super::video_size;

    #[test]
    fn video_keeps_the_screen_shape() {
        assert_eq!(video_size(800, 480), (800, 480));
        assert_eq!(video_size(1920, 1080), (1280, 720));
        assert_eq!(video_size(720, 1280), (720, 1280));
        assert_eq!(video_size(1080, 1920), (720, 1280));
    }
}
