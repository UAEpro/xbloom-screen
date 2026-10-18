//! The application: shared state plus the operations both front-ends use.
//!
//! The touchscreen UI and the admin HTTP API are two views of one [`App`]. Every action —
//! brew, cancel, edit a recipe, change a PIN, push the dial presets — goes through a method
//! here, so the rules (PIN checks, "remote start allowed?", validation) live in one place.
//! Changes are announced on an event bus that the UI subscribes to.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use xbloom_core::brew::Sample;
use xbloom_core::driver::{Timings, Transport};
use xbloom_core::sim::{SimConfig, SimFault};
use xbloom_core::Recipe;

use crate::auth::{valid_pin, Attempt, AuthState, Channel, PinHash};
use crate::controller::{BrewKind, Command, Controller, Link, MachineStatus};
use crate::history::{HistoryEntry, HistoryStore};
use crate::media::MediaStore;
use crate::presets::PresetStore;
use crate::recipes::{RecipeMeta, RecipeStore, StoredRecipe};
use crate::schedule::{
    Schedule, ScheduleInput, ScheduleStore, TimerRun, TimerStatus, CLOCK_SET_AFTER, COUNTDOWN_S, LATE_S,
};
use crate::settings::{parse_hhmm, LockMode, Settings, Wifi};
use crate::util::{local_time, now_unix, read_json, write_json};

/// Which stored data changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Data {
    /// Settings.
    Settings,
    /// The recipe library.
    Recipes,
    /// Brew history.
    History,
    /// Screensaver media.
    Media,
    /// Dial presets.
    Presets,
    /// Timed brews (schedules or the armed run).
    Timers,
}

/// Toast severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToastKind {
    /// Informational.
    Info,
    /// Something worked.
    Success,
    /// Heads-up.
    Warning,
    /// Something failed.
    Error,
}

/// Something happened.
#[derive(Debug, Clone)]
pub enum Event {
    /// The machine status changed.
    Status(MachineStatus),
    /// A live brew-curve point.
    Sample(Sample),
    /// A brew began (clear the live chart).
    BrewStarted,
    /// A brew finished; `entry` is set when it was recorded in history.
    BrewFinished {
        /// Outcome id.
        outcome: String,
        /// The history entry, for completed brews.
        entry: Option<HistoryEntry>,
    },
    /// Stored data changed (reload it).
    Changed(Data),
    /// Lock the screen now (requested from the admin panel).
    LockNow,
    /// The evening reminder: the next timed brew is within a day and not armed yet.
    TimerReminder(TimerRun),
    /// A short message for the screen.
    Toast {
        /// Text.
        message: String,
        /// Severity.
        kind: ToastKind,
    },
}

type Listener = Box<dyn Fn(&Event) + Send + Sync>;

/// What the device is, for the admin panel and the video converter.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlatformInfo {
    /// `raspberry-pi`, `linux`, `esp32s3`.
    pub name: String,
    /// Screen size, px.
    pub screen: (u32, u32),
    /// What the in-browser converter should produce for screensaver videos.
    pub video: VideoProfile,
    /// PBKDF2 rounds for new PINs (lower on an MCU).
    pub pin_iterations: u32,
    /// History entries kept.
    pub history_max: usize,
    /// Points per history curve.
    pub history_points: usize,
    /// Largest media upload, bytes.
    pub max_upload: u64,
    /// The firmware / app version.
    pub version: String,
    /// Whether the screen can be rotated from the settings (a kiosk Pi, the ESP32; not a
    /// desktop window).
    pub can_rotate: bool,
}

/// Screensaver video encoding target.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct VideoProfile {
    /// Frame width, px.
    pub width: u32,
    /// Frame height, px.
    pub height: u32,
    /// Frames per second.
    pub fps: u32,
    /// JPEG quality 0–1.
    pub quality: f32,
    /// Longest clip, seconds.
    pub max_seconds: u32,
}

/// Hooks into the host platform (backlight, restart, Wi-Fi, storage).
pub trait Platform: Send + Sync {
    /// Restart the device / app.
    fn restart(&self) {}
    /// Backlight, 0–100 %.
    fn set_brightness(&self, _percent: u8) {}
    /// Apply new Wi-Fi credentials (ESP32).
    fn wifi_changed(&self, _wifi: &Wifi) {}
    /// `(free, total)` bytes of the data storage.
    fn storage(&self) -> Option<(u64, u64)> {
        None
    }
    /// Addresses the admin panel is reachable at (`http://…`).
    fn admin_urls(&self) -> Vec<String> {
        Vec::new()
    }
    /// The device's own setup hotspot, while it is the way to reach the admin panel (ESP32
    /// without Wi-Fi credentials, or when the configured network is out of reach).
    fn setup_hotspot(&self) -> Option<Hotspot> {
        None
    }
}

/// A Wi-Fi network the device is hosting, for the screen to show (text + join QR code).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotspot {
    /// Network name.
    pub ssid: String,
    /// WPA2 passphrase.
    pub password: String,
}

/// A platform with no hooks (tests).
pub struct NoPlatform;
impl Platform for NoPlatform {}

/// Who is asking for a PIN check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinPurpose {
    /// Settings / admin panel: only the admin PIN.
    Admin,
    /// Unlock the screen or a brew: the user PIN or the admin PIN.
    Unlock,
}

/// The application.
pub struct App {
    /// Platform description.
    pub platform: PlatformInfo,
    data_dir: PathBuf,
    settings: Mutex<Settings>,
    /// Recipe library.
    pub recipes: Mutex<RecipeStore>,
    /// Brew history.
    pub history: Mutex<HistoryStore>,
    /// Dial presets.
    pub presets: Mutex<PresetStore>,
    /// Screensaver media.
    pub media: MediaStore,
    /// Timed brews.
    pub timers: Mutex<ScheduleStore>,
    /// When the last timer reminder was shown (once per evening).
    reminded: Mutex<Option<u64>>,
    /// The armed run whose countdown already woke the machine link.
    warmed: Mutex<Option<u64>>,
    stopping: AtomicBool,
    /// PIN throttles and web sessions.
    pub auth: Mutex<AuthState>,
    pub(crate) status: Mutex<MachineStatus>,
    listeners: Mutex<Vec<Listener>>,
    commands: Mutex<Sender<Command>>,
    hooks: Box<dyn Platform>,
}

impl App {
    /// Open the data directory, start the controller thread and return the app.
    ///
    /// `transport` is the platform's BLE link (used unless the simulator is enabled in
    /// settings). Files live under `data_dir`: `settings.json`, `recipes.json`,
    /// `history.json`, `presets.json` and `media/`.
    pub fn start(
        data_dir: &Path,
        platform: PlatformInfo,
        transport: Box<dyn Transport>,
        hooks: Box<dyn Platform>,
        sim: SimConfig,
    ) -> Arc<App> {
        Self::start_with_timings(data_dir, platform, transport, hooks, sim, Timings::default())
    }

    /// [`App::start`] with explicit driver pacing (tests).
    pub fn start_with_timings(
        data_dir: &Path,
        platform: PlatformInfo,
        transport: Box<dyn Transport>,
        hooks: Box<dyn Platform>,
        sim: SimConfig,
        timings: Timings,
    ) -> Arc<App> {
        let _ = std::fs::create_dir_all(data_dir);
        let mut settings: Settings = read_json(&data_dir.join("settings.json")).ok().flatten().unwrap_or_default();
        settings.sanitize();
        let media = MediaStore::new(data_dir.join("media"), platform.max_upload);
        media.cleanup();
        let (tx, rx) = mpsc::channel();
        let app = Arc::new(App {
            recipes: Mutex::new(RecipeStore::open(data_dir.join("recipes.json"))),
            history: Mutex::new(HistoryStore::open(
                data_dir.join("history.json"),
                platform.history_max,
                platform.history_points,
            )),
            presets: Mutex::new(PresetStore::open(data_dir.join("presets.json"))),
            media,
            timers: Mutex::new(ScheduleStore::open(data_dir.join("timers.json"))),
            reminded: Mutex::new(None),
            warmed: Mutex::new(None),
            stopping: AtomicBool::new(false),
            auth: Mutex::new(AuthState::default()),
            status: Mutex::new(MachineStatus { simulator: settings.machine.simulator, ..MachineStatus::default() }),
            settings: Mutex::new(settings),
            listeners: Mutex::new(Vec::new()),
            commands: Mutex::new(tx),
            data_dir: data_dir.to_path_buf(),
            platform,
            hooks,
        });
        let controller = Controller::new(Arc::clone(&app), rx, transport, sim, timings);
        thread::Builder::new()
            .name("xbloom-machine".into())
            .stack_size(48 * 1024)
            .spawn(move || controller.run())
            .expect("spawn the machine thread");
        let weak = Arc::downgrade(&app);
        thread::Builder::new()
            .name("xbloom-timers".into())
            .stack_size(12 * 1024)
            .spawn(move || timer_loop(weak))
            .expect("spawn the timer thread");
        app
    }

    /// The data directory.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Platform hooks.
    pub fn hooks(&self) -> &dyn Platform {
        self.hooks.as_ref()
    }

    // ── events ────────────────────────────────────────────────────────────────────────

    /// Subscribe to events. Listeners run on the emitting thread: keep them quick
    /// (e.g. post to the UI event loop) and never block.
    pub fn subscribe(&self, f: impl Fn(&Event) + Send + Sync + 'static) {
        self.listeners.lock().unwrap().push(Box::new(f));
    }

    /// Publish an event.
    pub fn emit(&self, ev: Event) {
        for l in self.listeners.lock().unwrap().iter() {
            l(&ev);
        }
    }

    /// Show a toast on the screen.
    pub fn toast(&self, message: &str, kind: ToastKind) {
        self.emit(Event::Toast { message: message.into(), kind });
    }

    fn send(&self, cmd: Command) {
        if self.commands.lock().unwrap().send(cmd).is_err() {
            log::error!("machine thread is gone");
        }
    }

    // ── settings ──────────────────────────────────────────────────────────────────────

    /// A copy of the settings.
    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    /// Change settings, persist, apply side effects (brightness, Wi-Fi, simulator), notify.
    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) -> Settings {
        let (before, after) = {
            let mut s = self.settings.lock().unwrap();
            let before = s.clone();
            f(&mut s);
            s.sanitize();
            (before, s.clone())
        };
        if before == after {
            return after;
        }
        if let Err(e) = write_json(&self.data_dir.join("settings.json"), &after) {
            log::error!("settings: save failed: {e}");
        }
        if before.appearance.brightness != after.appearance.brightness {
            self.hooks.set_brightness(after.appearance.brightness);
        }
        if before.wifi != after.wifi {
            self.hooks.wifi_changed(&after.wifi);
        }
        if before.machine.simulator != after.machine.simulator {
            self.send(Command::SetSimulator(after.machine.simulator));
        }
        self.emit(Event::Changed(Data::Settings));
        after
    }

    // ── PINs ──────────────────────────────────────────────────────────────────────────

    /// True until an admin PIN is set (first-run setup).
    pub fn needs_setup(&self) -> bool {
        self.settings.lock().unwrap().security.admin_pin.is_none()
    }

    /// First-run: set the admin PIN. Refused once one exists.
    pub fn setup_admin_pin(&self, pin: &str) -> Result<(), String> {
        if !valid_pin(pin) {
            return Err("The PIN must be 4–8 digits".into());
        }
        let hash = PinHash::new(pin, self.platform.pin_iterations);
        let mut done = false;
        self.update_settings(|s| {
            if s.security.admin_pin.is_none() {
                s.security.admin_pin = Some(hash);
                done = true;
            }
        });
        if done {
            Ok(())
        } else {
            Err("An admin PIN is already set".into())
        }
    }

    /// Check a PIN through the channel's brute-force throttle.
    pub fn check_pin(&self, channel: Channel, purpose: PinPurpose, pin: &str) -> Attempt {
        let s = self.settings();
        let mut hashes: Vec<&PinHash> = Vec::with_capacity(2);
        if let Some(a) = &s.security.admin_pin {
            hashes.push(a);
        }
        if purpose == PinPurpose::Unlock {
            if let Some(u) = &s.security.user_pin {
                hashes.push(u);
            }
        }
        if hashes.is_empty() {
            // No PIN configured yet: nothing to protect.
            return Attempt::Ok;
        }
        self.auth.lock().unwrap().check(channel, pin, &hashes)
    }

    /// Change the admin PIN (needs the current one). Ends all web sessions.
    pub fn change_admin_pin(&self, channel: Channel, current: &str, new: &str) -> Result<(), String> {
        if !valid_pin(new) {
            return Err("The new PIN must be 4–8 digits".into());
        }
        attempt_ok(self.check_pin(channel, PinPurpose::Admin, current))?;
        let hash = PinHash::new(new, self.platform.pin_iterations);
        self.update_settings(|s| s.security.admin_pin = Some(hash));
        self.auth.lock().unwrap().end_all_sessions();
        Ok(())
    }

    /// Set or clear the user (lock) PIN. Needs the admin PIN.
    pub fn set_user_pin(&self, channel: Channel, admin_pin: &str, new: Option<&str>) -> Result<(), String> {
        if let Some(p) = new {
            if !valid_pin(p) {
                return Err("The PIN must be 4–8 digits".into());
            }
        }
        attempt_ok(self.check_pin(channel, PinPurpose::Admin, admin_pin))?;
        let hash = new.map(|p| PinHash::new(p, self.platform.pin_iterations));
        self.update_settings(|s| s.security.user_pin = hash);
        Ok(())
    }

    /// Does starting a brew need a PIN right now?
    pub fn brew_needs_pin(&self) -> bool {
        let s = self.settings();
        s.lock_configured() && s.security.lock_mode == LockMode::Brew
    }

    // ── machine ───────────────────────────────────────────────────────────────────────

    /// A snapshot of the machine status.
    pub fn machine_status(&self) -> MachineStatus {
        self.status.lock().unwrap().clone()
    }

    /// Connect (to `address`, pairing it, or to the saved machine).
    pub fn connect(&self, address: Option<String>) {
        self.send(Command::Connect { address });
    }

    /// Disconnect and stay disconnected.
    pub fn disconnect(&self) {
        self.send(Command::Disconnect);
    }

    /// Forget the paired machine.
    pub fn forget_machine(&self) {
        self.send(Command::Forget);
    }

    /// Scan for machines.
    pub fn scan(&self) {
        self.send(Command::Scan);
    }

    /// Demo mode: make the simulated machine refuse the next brew.
    pub fn sim_fault(&self, fault: Option<SimFault>) {
        self.send(Command::SimFault(fault));
    }

    /// Brew a library recipe.
    ///
    /// `start: false` only **loads** it (the machine arms; approve on the machine).
    /// `start: true` also starts it remotely — refused if the admin turned remote start off.
    /// `dose_g` brews it at a different dose with the same ratio.
    pub fn brew(&self, recipe_id: &str, start: bool, dose_g: Option<i32>) -> Result<(), String> {
        self.brew_kind(recipe_id, start, dose_g, BrewKind::Recipe)
    }

    fn brew_kind(&self, recipe_id: &str, start: bool, dose_g: Option<i32>, kind: BrewKind) -> Result<(), String> {
        self.check_can_run(start)?;
        let stored = self.recipes.lock().unwrap().get(recipe_id).ok_or("No such recipe")?;
        let recipe = match dose_g {
            Some(d) if d != stored.recipe.dose_g => stored.recipe.scaled_to_dose(d),
            _ => stored.recipe,
        };
        recipe.validate().map_err(|e| e.0)?;
        self.send(Command::Brew { recipe: Box::new(recipe), recipe_id: Some(recipe_id.to_string()), start, kind });
        Ok(())
    }

    fn check_can_run(&self, start: bool) -> Result<(), String> {
        if start && !self.settings().security.allow_remote_start {
            return Err("Remote start is turned off — load the recipe and approve on the machine".into());
        }
        if self.machine_status().brew.as_ref().is_some_and(|b| !b.finished) {
            return Err("A brew is already running".into());
        }
        Ok(())
    }

    /// Rinse the paper filter: hot water through the empty filter, grinder off (see
    /// [`crate::settings::Rinse`]). Like a brew, `start: false` only loads it and you approve
    /// on the machine; `start: true` starts it remotely (refused if remote start is off). Not
    /// recorded in history.
    pub fn rinse(&self, start: bool) -> Result<(), String> {
        self.check_can_run(start)?;
        let recipe = self.settings().rinse.recipe();
        recipe.validate().map_err(|e| e.0)?;
        self.send(Command::Brew { recipe: Box::new(recipe), recipe_id: None, start, kind: BrewKind::Rinse });
        Ok(())
    }

    /// Cancel the running brew (or disarm a loaded recipe).
    pub fn cancel_brew(&self) {
        self.send(Command::Cancel);
    }

    /// Program dial presets A/B/C from the current assignment.
    pub fn push_presets(&self) -> Result<(), String> {
        let slots = self.presets.lock().unwrap().presets.slots.clone();
        let lib = self.recipes.lock().unwrap();
        let mut recipes = Vec::with_capacity(3);
        for (i, id) in slots.iter().enumerate() {
            let id = id.as_deref().ok_or_else(|| format!("Assign a recipe to slot {} first", ['A', 'B', 'C'][i]))?;
            recipes.push(lib.get(id).ok_or("A preset points at a deleted recipe")?.recipe);
        }
        drop(lib);
        let recipes: [Recipe; 3] = recipes.try_into().map_err(|_| "need three recipes")?;
        let scale = self.settings().machine.preset_scale;
        self.send(Command::SavePresets { recipes: Box::new(recipes), scale });
        Ok(())
    }

    /// Assign library recipes to the dial slots.
    pub fn set_presets(&self, slots: [Option<String>; 3]) -> Result<(), String> {
        {
            let lib = self.recipes.lock().unwrap();
            for id in slots.iter().flatten() {
                if lib.get(id).is_none() {
                    return Err(format!("No such recipe: {id}"));
                }
            }
        }
        {
            let mut p = self.presets.lock().unwrap();
            p.presets.slots = slots;
            p.save();
        }
        self.emit(Event::Changed(Data::Presets));
        Ok(())
    }

    // ── recipes (wrappers that notify) ────────────────────────────────────────────────

    /// Add a recipe.
    pub fn add_recipe(&self, recipe: Recipe, meta: RecipeMeta) -> Result<StoredRecipe, String> {
        let r = self.recipes.lock().unwrap().add(recipe, meta)?;
        self.emit(Event::Changed(Data::Recipes));
        Ok(r)
    }

    /// Replace a recipe. If the recipe itself changed (not just its favourite, colour …), an
    /// armed timer that brews it is disarmed: its checklist was confirmed for the old one.
    pub fn update_recipe(&self, id: &str, recipe: Recipe, meta: Option<RecipeMeta>) -> Result<StoredRecipe, String> {
        let (r, old) = {
            let mut lib = self.recipes.lock().unwrap();
            let old = lib.get(id).map(|s| s.recipe);
            (lib.update(id, recipe, meta)?, old)
        };
        self.emit(Event::Changed(Data::Recipes));
        if old.as_ref() != Some(&r.recipe) && self.timers.lock().unwrap().disarm_recipe(id) {
            self.toast("Recipe changed: arm its timer again at the machine", ToastKind::Info);
            self.emit(Event::Changed(Data::Timers));
        }
        Ok(r)
    }

    /// Delete a recipe (and clear any dial slot and timer that used it).
    pub fn delete_recipe(&self, id: &str) -> bool {
        let removed = self.recipes.lock().unwrap().delete(id);
        if removed {
            self.presets.lock().unwrap().forget_recipe(id);
            let timers = self.timers.lock().unwrap().forget_recipe(id);
            self.emit(Event::Changed(Data::Recipes));
            self.emit(Event::Changed(Data::Presets));
            if timers {
                self.emit(Event::Changed(Data::Timers));
            }
        }
        removed
    }

    /// Toggle a recipe's favourite flag.
    pub fn toggle_favorite(&self, id: &str) {
        if self.recipes.lock().unwrap().update_meta(id, |m| m.favorite = !m.favorite).is_some() {
            self.emit(Event::Changed(Data::Recipes));
        }
    }

    /// Notify that some store was changed directly.
    pub fn changed(&self, d: Data) {
        self.emit(Event::Changed(d));
    }

    /// Ask the screen to lock now.
    pub fn lock_screen(&self) {
        self.emit(Event::LockNow);
    }

    /// Stop the machine and timer threads (on shutdown).
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        self.send(Command::Shutdown);
    }

    // ── timed brews ───────────────────────────────────────────────────────────────────
    //
    // 🛑 A timer never brews on its own: each run is armed at the machine (the screen's
    // checklist and hold gesture call `arm_next_timer`, which nothing on the network reaches),
    // one arming covers one run, and the run counts down with a Cancel button first.

    /// The schedules.
    pub fn timer_list(&self) -> Vec<Schedule> {
        self.timers.lock().unwrap().list()
    }

    /// Add a schedule.
    pub fn add_timer(&self, input: ScheduleInput) -> Result<Schedule, String> {
        self.check_timer_recipe(&input.recipe_id)?;
        let s = self.timers.lock().unwrap().add(input, now_unix(), &local_time)?;
        self.emit(Event::Changed(Data::Timers));
        Ok(s)
    }

    /// Change a schedule (an armed run of it is disarmed if its time moved).
    pub fn update_timer(&self, id: &str, input: ScheduleInput) -> Result<Schedule, String> {
        self.check_timer_recipe(&input.recipe_id)?;
        let (s, disarmed) = self.timers.lock().unwrap().update(id, input, now_unix(), &local_time)?;
        if disarmed {
            self.toast("Timer changed: arm it again at the machine", ToastKind::Info);
        }
        self.emit(Event::Changed(Data::Timers));
        Ok(s)
    }

    /// Delete a schedule.
    pub fn delete_timer(&self, id: &str) -> bool {
        let removed = self.timers.lock().unwrap().delete(id);
        if removed {
            self.emit(Event::Changed(Data::Timers));
        }
        removed
    }

    fn check_timer_recipe(&self, recipe_id: &str) -> Result<(), String> {
        self.recipes.lock().unwrap().get(recipe_id).map(|_| ()).ok_or_else(|| "No such recipe".into())
    }

    /// Where timed brews stand at `now`.
    pub fn timer_status(&self, now: u64) -> TimerStatus {
        let store = self.timers.lock().unwrap();
        let armed = store.armed();
        let run = match &armed {
            Some(a) => store.get(&a.schedule_id).map(|s| (s.id, s.recipe_id, a.at, true)),
            None => store.next_run(now, &local_time).map(|r| (r.schedule_id, r.recipe_id, r.at, false)),
        };
        drop(store);
        let next = run.map(|(schedule_id, recipe_id, at, armed)| {
            let recipe_name =
                self.recipes.lock().unwrap().get(&recipe_id).map_or_else(|| "A recipe".into(), |r| r.recipe.name);
            TimerRun { schedule_id, recipe_id, recipe_name, at, armed }
        });
        let countdown_s = armed
            .as_ref()
            .filter(|a| a.at <= now + COUNTDOWN_S)
            .map(|a| a.at.saturating_sub(now))
            .filter(|_| now >= CLOCK_SET_AFTER);
        TimerStatus {
            next,
            countdown_s,
            clock_set: now >= CLOCK_SET_AFTER,
            remote_start: self.settings().security.allow_remote_start,
        }
    }

    /// 🔥 Arm the next timed run. **Only the screen calls this**, after its checklist (beans,
    /// filter and dripper, cup, water) and the hold gesture: there is deliberately no admin
    /// API route to it.
    pub fn arm_next_timer(&self, now: u64) -> Result<TimerRun, String> {
        if now < CLOCK_SET_AFTER {
            return Err("The clock isn't set yet: it syncs once the screen is on Wi-Fi".into());
        }
        if !self.settings().security.allow_remote_start {
            return Err("Timers start the machine from the screen: turn on remote start in Settings".into());
        }
        let run = {
            let store = self.timers.lock().unwrap();
            if store.armed().is_some() {
                return Err("A timer is already armed".into());
            }
            store.next_run(now, &local_time).ok_or("No timer is coming up")?
        };
        if run.at < now + COUNTDOWN_S {
            return Err("That timer is less than a minute away".into());
        }
        let recipe_name = self.recipes.lock().unwrap().get(&run.recipe_id).ok_or("No such recipe")?.recipe.name;
        self.timers.lock().unwrap().arm(&run, now);
        log::info!("timer: armed '{recipe_name}' for {}", run.at);
        self.emit(Event::Changed(Data::Timers));
        Ok(TimerRun { schedule_id: run.schedule_id, recipe_id: run.recipe_id, recipe_name, at: run.at, armed: true })
    }

    /// Disarm the armed run (screen Cancel, or the admin panel). Returns whether one was armed.
    pub fn disarm_timer(&self) -> bool {
        let was = self.timers.lock().unwrap().disarm();
        if was.is_some() {
            self.emit(Event::Changed(Data::Timers));
        }
        was.is_some()
    }

    /// One scheduler step at `now` (the timer thread calls this every second): start a due
    /// armed run, drop a late or orphaned one, switch off past one-off timers, and show the
    /// evening reminder.
    pub fn scheduler_tick(&self, now: u64) {
        if now < CLOCK_SET_AFTER {
            return;
        }
        let armed = self.timers.lock().unwrap().armed();
        if let Some(a) = armed {
            let schedule = self.timers.lock().unwrap().get(&a.schedule_id).filter(|s| s.enabled);
            match schedule {
                None => {
                    self.disarm_timer();
                }
                Some(s) if now >= a.at => {
                    {
                        let mut store = self.timers.lock().unwrap();
                        store.disarm();
                        store.finish_once(&s.id);
                    }
                    self.emit(Event::Changed(Data::Timers));
                    if now - a.at > LATE_S {
                        log::warn!("timer: missed the run at {} (now {now})", a.at);
                        self.toast("A timed brew was missed while the screen was off", ToastKind::Warning);
                    } else if let Err(e) = self.brew_kind(&s.recipe_id, true, None, BrewKind::Scheduled) {
                        log::warn!("timer: skipped: {e}");
                        self.toast(&format!("Timed brew skipped: {e}"), ToastKind::Error);
                    } else {
                        log::info!("timer: started the run at {}", a.at);
                    }
                }
                Some(_) if a.at <= now + COUNTDOWN_S => {
                    // The countdown is on: have the machine link ready when it ends.
                    let mut warmed = self.warmed.lock().unwrap();
                    if *warmed != Some(a.at) {
                        *warmed = Some(a.at);
                        if self.machine_status().link != Link::Connected {
                            self.connect(None);
                        }
                    }
                }
                Some(_) => {}
            }
        }
        if self.timers.lock().unwrap().expire_once(now) {
            self.emit(Event::Changed(Data::Timers));
        }
        self.maybe_remind(now);
    }

    fn maybe_remind(&self, now: u64) {
        let Some(at_minute) = parse_hhmm(&self.settings().timers.remind_at) else { return };
        let lt = local_time(now as i64);
        if lt.hour * 60 + lt.minute != at_minute {
            return;
        }
        let mut reminded = self.reminded.lock().unwrap();
        if reminded.is_some_and(|t| now < t + 3600) {
            return;
        }
        let status = self.timer_status(now);
        if let Some(run) = status.next.filter(|r| !r.armed && r.at <= now + 24 * 3600) {
            *reminded = Some(now);
            drop(reminded);
            self.emit(Event::TimerReminder(run));
        }
    }
}

/// The timer thread: a scheduler step every second until the app shuts down.
fn timer_loop(app: Weak<App>) {
    loop {
        thread::sleep(Duration::from_secs(1));
        let Some(app) = app.upgrade() else { return };
        if app.stopping.load(Ordering::Acquire) {
            return;
        }
        app.scheduler_tick(now_unix());
    }
}

/// Map a PIN attempt to a user-facing result.
pub fn attempt_ok(a: Attempt) -> Result<(), String> {
    match a {
        Attempt::Ok => Ok(()),
        Attempt::Wrong { remaining } if remaining > 0 => Err(format!("Wrong PIN — {remaining} tries left")),
        Attempt::Wrong { .. } => Err("Wrong PIN — one more wrong try locks PIN entry".into()),
        Attempt::Locked { retry_after } => {
            Err(format!("Too many wrong PINs — try again in {} s", retry_after.as_secs().max(1)))
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use xbloom_core::sim::SimTransport;

    pub fn platform() -> PlatformInfo {
        PlatformInfo {
            name: "test".into(),
            screen: (800, 480),
            video: VideoProfile { width: 800, height: 480, fps: 20, quality: 0.75, max_seconds: 30 },
            pin_iterations: 10,
            history_max: 50,
            history_points: 100,
            max_upload: 5_000_000,
            version: "test".into(),
            can_rotate: false,
        }
    }

    pub fn test_app(dir: &Path, simulator: bool) -> Arc<App> {
        if simulator {
            let mut s = Settings::default();
            s.machine.simulator = true;
            write_json(&dir.join("settings.json"), &s).unwrap();
        }
        let sim = SimConfig { speed: 20.0, grind_s: 4.0, cup_lift_s: 3.0, ..SimConfig::default() };
        App::start_with_timings(
            dir,
            platform(),
            Box::new(SimTransport::new(SimConfig::default())),
            Box::new(NoPlatform),
            sim,
            Timings::scaled(0.02),
        )
    }

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let t = Instant::now();
        while !f() {
            assert!(t.elapsed() < Duration::from_secs(25), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn setup_pin_once_and_check() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), false);
        assert!(app.needs_setup());
        // Before any PIN exists, nothing is protected.
        assert_eq!(app.check_pin(Channel::Device, PinPurpose::Admin, "0000"), Attempt::Ok);
        app.setup_admin_pin("2580").unwrap();
        assert!(app.setup_admin_pin("1111").is_err());
        assert_eq!(app.check_pin(Channel::Device, PinPurpose::Admin, "2580"), Attempt::Ok);
        assert!(matches!(app.check_pin(Channel::Device, PinPurpose::Admin, "1111"), Attempt::Wrong { .. }));
        app.set_user_pin(Channel::Device, "2580", Some("1234")).unwrap();
        assert_eq!(app.check_pin(Channel::Device, PinPurpose::Unlock, "1234"), Attempt::Ok);
        assert!(matches!(app.check_pin(Channel::Device, PinPurpose::Admin, "1234"), Attempt::Wrong { .. }));
        app.shutdown();
    }

    #[test]
    fn remote_start_can_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), true);
        app.update_settings(|s| s.security.allow_remote_start = false);
        let id = app.recipes.lock().unwrap().list()[0].id.clone();
        assert!(app.brew(&id, true, None).unwrap_err().contains("Remote start is turned off"));
        app.shutdown();
    }

    /// A daily timer two hours from now, for `recipe`.
    fn timer_in_two_hours(app: &App, recipe: &str) -> Schedule {
        let lt = local_time(now_unix() as i64);
        let time = crate::settings::hhmm((lt.hour * 60 + lt.minute + 120) % 1440);
        app.add_timer(ScheduleInput { recipe_id: recipe.into(), time, days: [true; 7], enabled: true }).unwrap()
    }

    #[test]
    fn a_rinse_runs_without_grinding_and_isnt_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), true);
        let finished = Arc::new(AtomicBool::new(false));
        let f2 = Arc::clone(&finished);
        app.subscribe(move |ev| {
            if let Event::BrewFinished { outcome, entry } = ev {
                assert_eq!((outcome.as_str(), entry.is_none()), ("completed", true));
                f2.store(true, Ordering::SeqCst);
            }
        });
        wait_until("connected", || app.machine_status().link == crate::controller::Link::Connected);
        app.update_settings(|s| s.rinse.ml = 60);
        app.rinse(true).unwrap();
        wait_until("rinse running", || app.machine_status().brew.is_some_and(|b| b.kind == BrewKind::Rinse));
        wait_until("rinse finished", || finished.load(Ordering::SeqCst));
        assert!(app.history.lock().unwrap().list().is_empty());
        // Like a brew, it can't start remotely when remote start is off.
        app.update_settings(|s| s.security.allow_remote_start = false);
        assert!(app.rinse(true).is_err());
        app.shutdown();
    }

    #[test]
    fn an_armed_timer_counts_down_then_brews_once() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), true);
        wait_until("connected", || app.machine_status().link == crate::controller::Link::Connected);
        let id = app.recipes.lock().unwrap().list()[0].id.clone();
        let t = timer_in_two_hours(&app, &id);
        let now = now_unix();
        let status = app.timer_status(now);
        let run = status.next.clone().unwrap();
        assert!(!run.armed && status.clock_set && status.remote_start && status.countdown_s.is_none());
        // Not armed: its time comes and goes without a brew.
        app.scheduler_tick(run.at);
        assert!(app.machine_status().brew.is_none());
        // Armed at the machine: a countdown in the last minute, then the brew.
        let armed = app.arm_next_timer(now).unwrap();
        assert_eq!((armed.schedule_id.as_str(), armed.at, armed.armed), (t.id.as_str(), run.at, true));
        assert!(app.arm_next_timer(now).is_err(), "one run at a time");
        assert_eq!(app.timer_status(run.at - 30).countdown_s, Some(30));
        app.scheduler_tick(run.at);
        wait_until("scheduled brew", || app.machine_status().brew.is_some_and(|b| b.kind == BrewKind::Scheduled));
        wait_until("brew finished", || app.machine_status().brew.is_some_and(|b| b.finished));
        assert_eq!(app.history.lock().unwrap().list().len(), 1);
        // The arming was used up: the next day's run isn't armed.
        let after = app.timer_status(run.at + 1).next.unwrap();
        assert!(!after.armed && after.at > run.at);
        app.shutdown();
    }

    #[test]
    fn a_late_or_disallowed_timer_does_not_brew() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), true);
        let id = app.recipes.lock().unwrap().list()[0].id.clone();
        timer_in_two_hours(&app, &id);
        let now = now_unix();
        let run = app.arm_next_timer(now).unwrap();
        // The screen was off past the run: dropped, not brewed late.
        app.scheduler_tick(run.at + LATE_S + 1);
        assert!(app.timers.lock().unwrap().armed().is_none());
        assert!(app.machine_status().brew.is_none());
        // Timers need remote start.
        app.update_settings(|s| s.security.allow_remote_start = false);
        assert!(app.arm_next_timer(now).unwrap_err().contains("remote start"));
        // Editing the recipe disarms its timer; touching only its favourite flag doesn't.
        app.update_settings(|s| s.security.allow_remote_start = true);
        app.arm_next_timer(now).unwrap();
        let stored = app.recipes.lock().unwrap().get(&id).unwrap();
        let meta = RecipeMeta { favorite: !stored.meta.favorite, ..stored.meta.clone() };
        app.update_recipe(&id, stored.recipe.clone(), Some(meta)).unwrap();
        assert!(app.timers.lock().unwrap().armed().is_some());
        let mut changed = stored.recipe.clone();
        changed.pours[0].temp_c -= 1;
        app.update_recipe(&id, changed, None).unwrap();
        assert!(app.timers.lock().unwrap().armed().is_none());
        // Deleting the recipe deletes its timer.
        app.delete_recipe(&id);
        assert!(app.timer_list().is_empty());
        app.shutdown();
    }

    #[test]
    fn simulated_brew_end_to_end_records_history() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), true);
        let finished = Arc::new(AtomicBool::new(false));
        let f2 = Arc::clone(&finished);
        app.subscribe(move |ev| {
            if let Event::BrewFinished { outcome, .. } = ev {
                assert_eq!(outcome, "completed");
                f2.store(true, Ordering::SeqCst);
            }
        });
        wait_until("connected", || app.machine_status().link == crate::controller::Link::Connected);
        let id = app.recipes.lock().unwrap().list()[0].id.clone();
        app.brew(&id, true, Some(12)).unwrap();
        wait_until("brew finished", || finished.load(Ordering::SeqCst));
        let h = app.history.lock().unwrap().list();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].dose_g, 12);
        assert!(h[0].water_g > 150.0, "water {}", h[0].water_g);
        assert_eq!(app.recipes.lock().unwrap().get(&id).unwrap().meta.brews, 1);
        app.shutdown();
    }

    #[test]
    fn demo_mode_switched_mid_brew_applies_when_the_brew_ends() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), true);
        let finished = Arc::new(AtomicBool::new(false));
        let f2 = Arc::clone(&finished);
        app.subscribe(move |ev| {
            if let Event::BrewFinished { .. } = ev {
                f2.store(true, Ordering::SeqCst);
            }
        });
        wait_until("connected", || app.machine_status().link == crate::controller::Link::Connected);
        let id = app.recipes.lock().unwrap().list()[0].id.clone();
        app.brew(&id, true, None).unwrap();
        // Arrives while the controller is busy brewing.
        app.update_settings(|s| s.machine.simulator = false);
        wait_until("brew finished", || finished.load(Ordering::SeqCst));
        wait_until("hardware transport back", || !app.machine_status().simulator);
        app.shutdown();
    }

    #[test]
    fn presets_push_through_the_simulator() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), true);
        wait_until("connected", || app.machine_status().link == crate::controller::Link::Connected);
        assert!(app.push_presets().unwrap_err().contains("slot A"));
        let ids: Vec<String> = app.recipes.lock().unwrap().list().iter().map(|r| r.id.clone()).collect();
        app.set_presets([Some(ids[0].clone()), Some(ids[1].clone()), Some(ids[2].clone())]).unwrap();
        assert!(app.presets.lock().unwrap().presets.dirty());
        app.push_presets().unwrap();
        wait_until("presets pushed", || !app.presets.lock().unwrap().presets.dirty());
        app.shutdown();
    }
}
