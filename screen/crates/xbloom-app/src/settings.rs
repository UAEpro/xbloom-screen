//! Device settings: appearance, screensaver, security/lock, machine pairing, Wi-Fi, the
//! rinse button and timed brews.
//!
//! Persisted as `settings.json`. Every field has a default (`#[serde(default)]`), so a file
//! from an older version loads cleanly and unknown future fields are ignored.
//!
//! Secrets (PIN hashes, the Wi-Fi password) never leave the device: the admin API returns
//! [`Settings::public`], and accepts changes through [`SettingsPatch`], which cannot touch PINs
//! (they have their own, current-PIN-gated endpoint).

use serde::{Deserialize, Serialize};
use xbloom_core::recipe::{Pattern, Pour};
use xbloom_core::Recipe;

use crate::auth::PinHash;

/// Colour scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Light (the default — bright kitchens).
    #[default]
    Light,
    /// Dark.
    Dark,
    /// Light by day, dark from 19:00 to 07:00.
    Auto,
}

/// What the idle screen shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScreensaverMode {
    /// No screensaver; just dim / sleep.
    Off,
    /// A large clock over the theme background.
    #[default]
    Clock,
    /// Loop the selected video.
    Video,
    /// Cycle the uploaded photos.
    Slideshow,
}

/// Who may do what without a PIN.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LockMode {
    /// Anyone can brew; only settings need the admin PIN.
    #[default]
    Off,
    /// Browsing is free, but starting a brew needs a PIN (child / office lock).
    Brew,
    /// The whole screen is locked behind a PIN after it wakes.
    Screen,
}

/// Look and feel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    /// Colour scheme.
    pub theme: Theme,
    /// Accent colour, `#rrggbb`.
    pub accent: String,
    /// Backlight while in use, 10–100 %.
    pub brightness: u8,
    /// Backlight while the screensaver runs, 0–100 %.
    pub dim_brightness: u8,
    /// 24-hour clock.
    pub clock_24h: bool,
    /// Interface size, % (70–200): bigger text and controls on dense panels. Capped on the
    /// device so the layout never gets smaller than 480 × 272.
    pub ui_scale: u16,
    /// Picture and touch rotation, clockwise degrees (0, 90, 180, 270), to match how the
    /// panel is mounted. Applied at start-up: changing it restarts the screen app.
    pub rotation: u16,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: Theme::Light,
            accent: "#c8743a".into(),
            brightness: 80,
            dim_brightness: 25,
            clock_24h: true,
            ui_scale: 100,
            rotation: 0,
        }
    }
}

/// Idle behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Screensaver {
    /// What to show.
    pub mode: ScreensaverMode,
    /// Idle seconds before it starts (15–3600).
    pub idle_s: u32,
    /// The video (`*.xbv`) to loop in video mode.
    pub video: Option<String>,
    /// Seconds per photo in slideshow mode (3–120).
    pub slide_s: u32,
    /// Overlay the clock on video / photos.
    pub clock_overlay: bool,
    /// Turn the backlight off after this many seconds of screensaver (0 = never).
    pub sleep_after_s: u32,
}

impl Default for Screensaver {
    fn default() -> Self {
        Self {
            mode: ScreensaverMode::Clock,
            idle_s: 120,
            video: None,
            slide_s: 8,
            clock_overlay: true,
            sleep_after_s: 0,
        }
    }
}

/// Lock / PIN configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Security {
    /// What the lock protects.
    pub lock_mode: LockMode,
    /// Re-lock after this many idle seconds once unlocked (30–3600).
    pub auto_lock_s: u32,
    /// Show the remote **Start** button (brew from the screen). When off, the screen only
    /// loads recipes and the human approves on the machine.
    pub allow_remote_start: bool,
    /// How long the Brew button must be held, ms (500–3000) — the deliberate-gesture gate.
    pub hold_to_brew_ms: u32,
    /// Admin PIN (settings, admin panel). `None` until first-run setup.
    pub admin_pin: Option<PinHash>,
    /// Optional user PIN for the lock (the admin PIN always unlocks too).
    pub user_pin: Option<PinHash>,
}

impl Default for Security {
    fn default() -> Self {
        Self {
            lock_mode: LockMode::Off,
            auto_lock_s: 300,
            allow_remote_start: true,
            hold_to_brew_ms: 1200,
            admin_pin: None,
            user_pin: None,
        }
    }
}

/// The paired machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MachineSettings {
    /// Saved BLE address (skip the scan on boot).
    pub address: Option<String>,
    /// Its advertised name.
    pub name: Option<String>,
    /// Connect on boot and hold the link open (fast brews; reconnects on drop).
    pub auto_connect: bool,
    /// Drive the built-in simulator instead of real hardware (demo mode).
    pub simulator: bool,
    /// Store dial presets with the on-brew scale enabled.
    pub preset_scale: [bool; 3],
}

impl Default for MachineSettings {
    fn default() -> Self {
        Self { address: None, name: None, auto_connect: true, simulator: false, preset_scale: [true; 3] }
    }
}

/// The rinse-filter button: hot water through the empty paper filter, grinder off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rinse {
    /// Water, ml (30–250).
    pub ml: i32,
    /// Temperature, °C (40–95).
    pub temp_c: i32,
}

impl Default for Rinse {
    fn default() -> Self {
        Self { ml: 100, temp_c: 95 }
    }
}

impl Rinse {
    /// The rinse as something the machine can run: grinder off, two spiral pours (a recipe
    /// needs at least two) that wet the whole paper, no pauses. The dose only keeps the brew
    /// ratio inside the machine's 1:25.5 limit; nothing is ground.
    pub fn recipe(&self) -> Recipe {
        let pour = |ml| Pour {
            label: Some("Rinse".into()),
            ml,
            temp_c: self.temp_c,
            pattern: Pattern::Spiral,
            pause_s: 0,
            rpm: 80,
            flow_ml_s: 3.5,
            agitation: false,
        };
        let first = self.ml / 2;
        Recipe {
            name: "Rinse".into(),
            dose_g: ((self.ml + 24) / 25).clamp(1, 18),
            grind: 0,
            ratio: None,
            stage_temps: [110.0, 90.0],
            kind: Some("rinse".into()),
            dripper: None,
            water_ml: None,
            hot_water_ml: None,
            ice_g: None,
            time: None,
            note: None,
            pours: vec![pour(first), pour(self.ml - first)],
        }
    }
}

/// Timed brews.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Timers {
    /// Local time ("21:00") at which the screen reminds you to prepare and arm the next timed
    /// brew if it is within a day and not armed yet; empty = no reminder.
    pub remind_at: String,
}

impl Default for Timers {
    fn default() -> Self {
        Self { remind_at: "21:00".into() }
    }
}

/// Minutes after midnight for a `HH:MM` local time.
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// `HH:MM` for minutes after midnight.
pub fn hhmm(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60 % 24, minutes % 60)
}

/// Wi-Fi credentials (ESP32 only; a Raspberry Pi uses the OS network config).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Wifi {
    /// Network name.
    pub ssid: String,
    /// Passphrase (never returned by the API).
    pub password: String,
}

/// All device settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Shown in the admin panel and the network.
    pub device_name: String,
    /// First-run setup finished.
    pub setup_complete: bool,
    /// POSIX TZ string for the clock (ESP32; the Pi uses the system zone).
    pub timezone: String,
    /// Look and feel.
    pub appearance: Appearance,
    /// Idle behaviour.
    pub screensaver: Screensaver,
    /// Lock and PINs.
    pub security: Security,
    /// The paired machine.
    pub machine: MachineSettings,
    /// Wi-Fi (ESP32).
    pub wifi: Wifi,
    /// The rinse-filter button.
    pub rinse: Rinse,
    /// Timed brews.
    pub timers: Timers,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            device_name: "xBloom Screen".into(),
            setup_complete: false,
            timezone: "UTC0".into(),
            appearance: Appearance::default(),
            screensaver: Screensaver::default(),
            security: Security::default(),
            machine: MachineSettings::default(),
            wifi: Wifi::default(),
            rinse: Rinse::default(),
            timers: Timers::default(),
        }
    }
}

impl Settings {
    /// The settings as the admin API shows them: no PIN hashes, no Wi-Fi password.
    pub fn public(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(sec) = v.get_mut("security").and_then(|s| s.as_object_mut()) {
            sec.remove("admin_pin");
            sec.remove("user_pin");
            sec.insert("has_admin_pin".into(), self.security.admin_pin.is_some().into());
            sec.insert("has_user_pin".into(), self.security.user_pin.is_some().into());
        }
        if let Some(w) = v.get_mut("wifi").and_then(|s| s.as_object_mut()) {
            w.remove("password");
            w.insert("has_password".into(), (!self.wifi.password.is_empty()).into());
        }
        v
    }

    /// Clamp every numeric field into its valid range (after a patch or a hand-edited file).
    pub fn sanitize(&mut self) {
        let a = &mut self.appearance;
        a.brightness = a.brightness.clamp(10, 100);
        a.dim_brightness = a.dim_brightness.min(100);
        // Whole 10 % steps.
        a.ui_scale = (a.ui_scale.clamp(70, 200) + 5) / 10 * 10;
        if !matches!(a.rotation, 0 | 90 | 180 | 270) {
            a.rotation = 0;
        }
        if !is_hex_color(&a.accent) {
            a.accent = Appearance::default().accent;
        }
        let s = &mut self.screensaver;
        s.idle_s = s.idle_s.clamp(15, 3600);
        s.slide_s = s.slide_s.clamp(3, 120);
        s.sleep_after_s = s.sleep_after_s.min(24 * 3600);
        let sec = &mut self.security;
        sec.auto_lock_s = sec.auto_lock_s.clamp(30, 3600);
        sec.hold_to_brew_ms = sec.hold_to_brew_ms.clamp(500, 3000);
        self.rinse.ml = self.rinse.ml.clamp(30, 250);
        self.rinse.temp_c = self.rinse.temp_c.clamp(40, 95);
        let remind = self.timers.remind_at.trim();
        self.timers.remind_at =
            if remind.is_empty() { String::new() } else { parse_hhmm(remind).map(hhmm).unwrap_or_default() };
        if self.device_name.trim().is_empty() {
            self.device_name = Settings::default().device_name;
        }
        if let Some((i, _)) = self.device_name.char_indices().nth(40) {
            self.device_name.truncate(i);
        }
        // A POSIX TZ string ("UTC0", "CET-1CEST,M3.5.0,M10.5.0/3"): short printable ASCII.
        let tz = self.timezone.trim();
        if tz.is_empty() || tz.len() > 64 || !tz.chars().all(|c| c.is_ascii_graphic()) {
            self.timezone = Settings::default().timezone;
        } else if tz.len() != self.timezone.len() {
            self.timezone = tz.to_string();
        }
    }

    /// Apply a patch from the admin API. Returns true if anything changed.
    pub fn apply(&mut self, patch: SettingsPatch) -> bool {
        let before = self.clone();
        if let Some(v) = patch.device_name {
            self.device_name = v;
        }
        if let Some(v) = patch.timezone {
            self.timezone = v;
        }
        if let Some(v) = patch.appearance {
            self.appearance = v;
        }
        if let Some(v) = patch.screensaver {
            self.screensaver = v;
        }
        if let Some(p) = patch.security {
            if let Some(v) = p.lock_mode {
                self.security.lock_mode = v;
            }
            if let Some(v) = p.auto_lock_s {
                self.security.auto_lock_s = v;
            }
            if let Some(v) = p.allow_remote_start {
                self.security.allow_remote_start = v;
            }
            if let Some(v) = p.hold_to_brew_ms {
                self.security.hold_to_brew_ms = v;
            }
        }
        if let Some(p) = patch.machine {
            if let Some(v) = p.auto_connect {
                self.machine.auto_connect = v;
            }
            if let Some(v) = p.simulator {
                self.machine.simulator = v;
            }
            if let Some(v) = p.preset_scale {
                self.machine.preset_scale = v;
            }
        }
        if let Some(w) = patch.wifi {
            self.wifi.ssid = w.ssid;
            if let Some(pw) = w.password {
                self.wifi.password = pw;
            }
        }
        if let Some(v) = patch.rinse {
            self.rinse = v;
        }
        if let Some(v) = patch.timers {
            self.timers = v;
        }
        self.sanitize();
        *self != before
    }

    /// The lock needs a PIN: the user PIN if set, else the admin PIN.
    pub fn lock_configured(&self) -> bool {
        self.security.lock_mode != LockMode::Off
            && (self.security.user_pin.is_some() || self.security.admin_pin.is_some())
    }
}

fn is_hex_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// Security fields the admin API may change (never the PINs themselves).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SecurityPatch {
    /// What the lock protects.
    pub lock_mode: Option<LockMode>,
    /// Re-lock delay.
    pub auto_lock_s: Option<u32>,
    /// Show the remote Start button.
    pub allow_remote_start: Option<bool>,
    /// Hold-to-brew duration.
    pub hold_to_brew_ms: Option<u32>,
}

/// Machine fields the admin API may change (pairing goes through the machine endpoints).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MachinePatch {
    /// Connect on boot.
    pub auto_connect: Option<bool>,
    /// Demo mode.
    pub simulator: Option<bool>,
    /// Scale flag per dial preset.
    pub preset_scale: Option<[bool; 3]>,
}

/// Wi-Fi change; omit the password to keep the current one.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WifiPatch {
    /// Network name.
    pub ssid: String,
    /// New passphrase.
    pub password: Option<String>,
}

/// A partial settings update from the admin API.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsPatch {
    /// Device name.
    pub device_name: Option<String>,
    /// POSIX TZ.
    pub timezone: Option<String>,
    /// Look and feel (whole section).
    pub appearance: Option<Appearance>,
    /// Idle behaviour (whole section).
    pub screensaver: Option<Screensaver>,
    /// Lock settings (field by field).
    pub security: Option<SecurityPatch>,
    /// Machine settings (field by field).
    pub machine: Option<MachinePatch>,
    /// Wi-Fi.
    pub wifi: Option<WifiPatch>,
    /// The rinse-filter button (whole section).
    pub rinse: Option<Rinse>,
    /// Timed brews (whole section).
    pub timers: Option<Timers>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_view_hides_secrets() {
        let mut s = Settings::default();
        s.security.admin_pin = Some(PinHash::new("1234", 10));
        s.wifi.password = "hunter2".into();
        let v = s.public().to_string();
        assert!(!v.contains("hunter2"));
        assert!(!v.contains("salt"));
        assert!(v.contains("\"has_admin_pin\":true"));
    }

    #[test]
    fn interface_size_and_rotation_are_kept_valid() {
        let mut s = Settings::default();
        s.appearance.ui_scale = 500;
        s.appearance.rotation = 45;
        s.sanitize();
        assert_eq!((s.appearance.ui_scale, s.appearance.rotation), (200, 0));
        s.appearance.ui_scale = 124;
        s.appearance.rotation = 270;
        s.sanitize();
        assert_eq!((s.appearance.ui_scale, s.appearance.rotation), (120, 270));
        s.appearance.ui_scale = 10;
        s.sanitize();
        assert_eq!(s.appearance.ui_scale, 70);
    }

    #[test]
    fn patch_clamps_and_reports_change() {
        let mut s = Settings::default();
        let patch: SettingsPatch = serde_json::from_str(
            r##"{"appearance":{"theme":"dark","accent":"nope","brightness":250},"security":{"hold_to_brew_ms":1}}"##,
        )
        .unwrap();
        assert!(s.apply(patch));
        assert_eq!(s.appearance.theme, Theme::Dark);
        assert_eq!(s.appearance.brightness, 100);
        assert_eq!(s.appearance.accent, "#c8743a");
        assert_eq!(s.security.hold_to_brew_ms, 500);
    }

    #[test]
    fn names_and_zones_are_sanitized() {
        let mut s = Settings {
            device_name: "Café ".repeat(20),
            timezone: " CET-1CEST,M3.5.0,M10.5.0/3 ".into(),
            ..Settings::default()
        };
        s.sanitize();
        assert_eq!(s.device_name.chars().count(), 40);
        assert_eq!(s.timezone, "CET-1CEST,M3.5.0,M10.5.0/3");
        s.timezone = "Europe/Zürich; rm".into();
        s.sanitize();
        assert_eq!(s.timezone, "UTC0");
    }

    #[test]
    fn rinse_and_reminder_are_kept_valid() {
        let mut s = Settings {
            rinse: Rinse { ml: 5000, temp_c: 20 },
            timers: Timers { remind_at: " 7:5 ".into() },
            ..Settings::default()
        };
        s.sanitize();
        assert_eq!((s.rinse.ml, s.rinse.temp_c, s.timers.remind_at.as_str()), (250, 40, "07:05"));
        s.timers.remind_at = "25:00".into();
        s.sanitize();
        assert_eq!(s.timers.remind_at, "");
        assert_eq!((parse_hhmm("23:59"), parse_hhmm("07:60"), parse_hhmm("7")), (Some(1439), None, None));
        assert_eq!(hhmm(425), "07:05");
    }

    #[test]
    fn every_rinse_is_a_valid_no_grind_run() {
        for ml in [30, 31, 100, 249, 250] {
            for temp_c in [40, 95] {
                let r = Rinse { ml, temp_c }.recipe();
                r.validate().unwrap_or_else(|e| panic!("{ml} ml at {temp_c}°: {e}"));
                assert!(r.no_grind());
                assert_eq!(r.total_water_ml(), ml);
            }
        }
    }

    #[test]
    fn old_files_load_with_defaults() {
        let s: Settings = serde_json::from_str(r#"{"device_name":"Bar"}"#).unwrap();
        assert_eq!(s.device_name, "Bar");
        assert_eq!(s.screensaver.idle_s, 120);
    }

    #[test]
    fn patch_cannot_set_pins() {
        let r: Result<SettingsPatch, _> =
            serde_json::from_str(r#"{"security":{"admin_pin":{"salt":"","hash":"","iterations":1}}}"#);
        // Unknown security fields are ignored, so the PIN can't sneak in.
        let mut s = Settings::default();
        s.apply(r.unwrap());
        assert!(s.security.admin_pin.is_none());
    }
}
