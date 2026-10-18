//! Wiring between the [`App`] and the Slint UI.
//!
//! [`bind`] fills the UI from the app, implements every `AppState` callback by calling the app,
//! and subscribes to app events (posted onto the UI thread with `invoke_from_event_loop`). It
//! also owns the screen-only behaviour: the idle timer (screensaver, dimming, sleep, auto-lock),
//! the PIN flows, the toast, and the live chart.
//!
//! 🛑 The only path to a remote start is `AppState.start-brew(true)`, which the UI raises only
//! from the hold-to-brew gesture; it then goes through [`App::brew`], which refuses when remote
//! start is turned off, and through the brew-lock PIN when one is configured.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};
use xbloom_app::app::attempt_ok;
use xbloom_app::auth::{valid_pin, Attempt, Channel};
use xbloom_app::settings::{LockMode, ScreensaverMode, Settings, Theme as ThemeMode};
use xbloom_app::{App, Data, Event, Link, MachineStatus, PinPurpose, ToastKind};
use xbloom_core::brew::Sample;

use crate::format::{self, ChartScale};
use crate::saver::{SaverContent, SaverPlayer};
use crate::{clock, qr};
use crate::{AppState, AppWindow, BrewInfo, LinkState, MediaRow, Overlay, Page, ScanRow, SlotRow, Theme, ToastInfo};

/// What the PIN pad is collecting right now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum PinFlow {
    #[default]
    None,
    UnlockScreen,
    Settings,
    BrewUnlock {
        start: bool,
    },
    ChangeAdminCurrent,
    ChangeAdminNew {
        current: String,
    },
    ChangeAdminConfirm {
        current: String,
        new: String,
    },
    UserNew,
    UserConfirm {
        new: String,
    },
    SetupNew,
    SetupConfirm {
        new: String,
    },
}

struct Ui {
    app: Arc<App>,
    weak: Weak<AppWindow>,
    pin: PinFlow,
    pin_buf: String,
    /// The admin PIN, remembered only while the settings page is open.
    admin_pin: Option<String>,
    last_activity: Instant,
    saver_started: Option<Instant>,
    player: Option<SaverPlayer>,
    samples: Vec<Sample>,
    chart_dirty: bool,
    brew_target: f32,
    brew_expected_s: f32,
    dismissed: bool,
    toast_timer: Timer,
    last_device_refresh: Instant,
    dimmed: bool,
}

thread_local! {
    static UI: RefCell<Option<Rc<RefCell<Ui>>>> = const { RefCell::new(None) };
}

fn link_state(l: Link) -> LinkState {
    match l {
        Link::Connected => LinkState::Connected,
        Link::Connecting => LinkState::Connecting,
        Link::Disconnected => LinkState::Disconnected,
    }
}

impl Ui {
    fn with(&self, f: impl FnOnce(&AppWindow)) {
        if let Some(ui) = self.weak.upgrade() {
            f(&ui);
        }
    }

    // ── loading ───────────────────────────────────────────────────────────────────────

    fn load_all(&mut self) {
        self.apply_settings(&self.app.settings());
        self.load_recipes();
        self.load_history();
        self.load_presets();
        self.load_media();
        self.load_device();
        self.apply_status(&self.app.machine_status());
        self.tick_clock();
    }

    fn apply_settings(&mut self, s: &Settings) {
        let night = clock::is_night(clock::now());
        let brew_needs_pin = self.app.brew_needs_pin();
        let paired = s.machine.address.is_some();
        self.with(|ui| {
            let t = ui.global::<Theme>();
            t.set_dark(match s.appearance.theme {
                ThemeMode::Light => false,
                ThemeMode::Dark => true,
                ThemeMode::Auto => night,
            });
            if let Some(c) = format::parse_hex(&s.appearance.accent) {
                t.set_accent(c);
            }
            let g = ui.global::<AppState>();
            g.set_device_name(s.device_name.as_str().into());
            g.set_theme_mode(match s.appearance.theme {
                ThemeMode::Light => 0,
                ThemeMode::Dark => 1,
                ThemeMode::Auto => 2,
            });
            g.set_brightness(i32::from(s.appearance.brightness));
            g.set_clock_24h(s.appearance.clock_24h);
            g.set_ui_scale(i32::from(s.appearance.ui_scale));
            g.set_rotation(i32::from(s.appearance.rotation / 90));
            g.set_allow_remote_start(s.security.allow_remote_start);
            g.set_hold_ms(s.security.hold_to_brew_ms as i32);
            g.set_lock_mode(match s.security.lock_mode {
                LockMode::Off => 0,
                LockMode::Brew => 1,
                LockMode::Screen => 2,
            });
            g.set_brew_needs_pin(brew_needs_pin);
            g.set_has_user_pin(s.security.user_pin.is_some());
            g.set_auto_lock_min(((s.security.auto_lock_s + 30) / 60).max(1) as i32);
            g.set_saver_mode(match s.screensaver.mode {
                ScreensaverMode::Off => 0,
                ScreensaverMode::Clock => 1,
                ScreensaverMode::Video => 2,
                ScreensaverMode::Slideshow => 3,
            });
            g.set_saver_idle_min(((s.screensaver.idle_s + 30) / 60).max(1) as i32);
            g.set_saver_video(s.screensaver.video.clone().unwrap_or_default().into());
            g.set_saver_clock_overlay(s.screensaver.clock_overlay);
            g.set_auto_connect(s.machine.auto_connect);
            g.set_simulator(s.machine.simulator);
            g.set_paired(paired);
            let needs_setup = !s.setup_complete;
            g.set_needs_setup(needs_setup);
        });
        self.apply_scale(s.appearance.ui_scale);
        self.tick_clock();
    }

    fn apply_scale(&self, percent: u16) {
        self.with(|ui| {
            let max = apply_interface_size(ui, percent);
            ui.global::<AppState>().set_ui_scale_max(max);
        });
    }

    fn load_recipes(&mut self) {
        let (visible, all) = {
            let r = self.app.recipes.lock().unwrap();
            (r.visible(), r.list().len())
        };
        let cards: Vec<_> = visible.iter().enumerate().map(|(i, s)| format::recipe_card(s, i)).collect();
        self.with(|ui| {
            let g = ui.global::<AppState>();
            let cur = g.get_current().id;
            if let Some(c) = cards.iter().find(|c| c.id == cur) {
                g.set_current(c.clone());
            }
            g.set_recipes(ModelRc::new(VecModel::from(cards)));
            g.set_recipe_count(all as i32);
        });
        self.load_presets();
    }

    fn load_history(&mut self) {
        let list = self.app.history.lock().unwrap().list();
        let h24 = self.app.settings().appearance.clock_24h;
        let rows: Vec<_> =
            list.iter().map(|e| format::history_row(e, clock::short_text(clock::local(e.ts as i64), h24))).collect();
        let n = rows.len() as i32;
        self.with(|ui| {
            let g = ui.global::<AppState>();
            g.set_history(ModelRc::new(VecModel::from(rows)));
            g.set_brew_count(n);
            if g.get_history_selected() >= n {
                g.set_history_selected(0);
            }
        });
        let sel = self.weak.upgrade().map_or(0, |ui| ui.global::<AppState>().get_history_selected());
        self.select_history(sel);
    }

    fn select_history(&mut self, i: i32) {
        let list = self.app.history.lock().unwrap().list();
        let Some(summary) = list.get(i.max(0) as usize) else { return };
        let Some(e) = self.app.history.lock().unwrap().get(&summary.id) else { return };
        let h24 = self.app.settings().appearance.clock_24h;
        let samples = format::curve_samples(&e);
        let scale = ChartScale::fit(e.water_g.max(1.0), e.duration_s, &samples);
        let (water, coffee) = format::chart_paths(&samples, scale);
        let grind = if e.grind == 0 { "pre-ground".to_string() } else { format!("grind {}", e.grind) };
        let detail = format!(
            "{}  ·  {} g · {} · {}\n{} g water · {} g in cup · {} · {}",
            clock::short_text(clock::local(e.ts as i64), h24),
            e.dose_g,
            format::ratio_text(e.ratio),
            grind,
            e.water_g.round(),
            e.coffee_g.round(),
            format::mmss(e.duration_s),
            if e.remote_start { "started from the screen" } else { "started on the machine" },
        );
        self.with(|ui| {
            let g = ui.global::<AppState>();
            g.set_history_selected(i);
            g.set_history_title(e.recipe.as_str().into());
            g.set_history_detail(detail.into());
            g.set_history_water(water.into());
            g.set_history_coffee(coffee.into());
        });
    }

    fn load_presets(&mut self) {
        let p = self.app.presets.lock().unwrap().presets.clone();
        let scale = self.app.settings().machine.preset_scale;
        let lib = self.app.recipes.lock().unwrap();
        let rows: Vec<SlotRow> = (0..3)
            .map(|i| {
                let r = p.slots[i].as_deref().and_then(|id| lib.get(id));
                SlotRow {
                    slot: ["A", "B", "C"][i].into(),
                    recipe_id: r.as_ref().map_or_else(SharedString::new, |r| r.id.as_str().into()),
                    recipe: r.as_ref().map_or_else(SharedString::new, |r| r.recipe.name.as_str().into()),
                    detail: r.as_ref().map_or_else(
                        || "Tap “Choose recipe”".into(),
                        |r| {
                            let g = if r.recipe.no_grind() {
                                "pre-ground".into()
                            } else {
                                format!("grind {}", r.recipe.grind)
                            };
                            format!(
                                "{} g · {} · {}",
                                r.recipe.dose_g,
                                format::ratio_text(r.recipe.effective_ratio()),
                                g
                            )
                            .into()
                        },
                    ),
                    scale: scale[i],
                }
            })
            .collect();
        drop(lib);
        let status = if !p.complete() {
            "Assign a recipe to all three slots, then save them to the machine.".to_string()
        } else if p.dirty() {
            "Changes aren't on the machine yet — save to program the dial.".to_string()
        } else {
            let ago = xbloom_app::util::now_unix().saturating_sub(p.pushed_at.unwrap_or(0));
            format!(
                "On the machine since {}. The xBloom phone app can overwrite these — save again to restore.",
                if ago < 120 {
                    "just now".to_string()
                } else if ago < 7200 {
                    format!("{} min ago", ago / 60)
                } else {
                    format!("{} h ago", ago / 3600)
                }
            )
        };
        let dirty = p.dirty() || !p.complete();
        self.with(|ui| {
            let g = ui.global::<AppState>();
            g.set_slots(ModelRc::new(VecModel::from(rows)));
            g.set_slots_dirty(dirty);
            g.set_slots_status(status.into());
        });
    }

    fn load_media(&mut self) {
        let rows: Vec<MediaRow> = self
            .app
            .media
            .list()
            .into_iter()
            .map(|m| {
                let video = m.kind == xbloom_app::media::MediaKind::Video;
                MediaRow {
                    name: m.name.as_str().into(),
                    detail: if video {
                        format!("{}×{} · {:.0} s", m.width, m.height, m.frames as f32 / m.fps.max(1.0)).into()
                    } else {
                        format!("{}×{} photo", m.width, m.height).into()
                    },
                    video,
                }
            })
            .collect();
        self.with(|ui| ui.global::<AppState>().set_media(ModelRc::new(VecModel::from(rows))));
    }

    fn load_device(&mut self) {
        self.last_device_refresh = Instant::now();
        let p = &self.app.platform;
        let platform = match p.name.as_str() {
            "raspberry-pi" => "Raspberry Pi".to_string(),
            "esp32s3" => "ESP32-S3".to_string(),
            other => other.to_string(),
        };
        let storage = self.app.hooks().storage().map_or_else(
            || "—".to_string(),
            |(free, total)| format!("{} free of {}", format::bytes(free), format::bytes(total)),
        );
        let url = self.app.hooks().admin_urls().into_iter().next().unwrap_or_default();
        // On its own setup Wi-Fi the QR code joins the network; otherwise it opens the panel.
        let hotspot = self.app.hooks().setup_hotspot();
        let (qr_text, hint) = match &hotspot {
            Some(h) => (
                qr::wifi_join(&h.ssid, &h.password),
                format!("Scan to join the Wi-Fi “{}” (password {}), then open the address above.", h.ssid, h.password),
            ),
            None => (url.clone(), String::new()),
        };
        self.with(|ui| {
            let g = ui.global::<AppState>();
            g.set_can_rotate(p.can_rotate);
            g.set_version(p.version.as_str().into());
            g.set_platform(format!("{platform} · {}×{}", p.screen.0, p.screen.1).into());
            g.set_storage(storage.into());
            if g.get_admin_url().as_str() != url || g.get_admin_hint().as_str() != hint {
                g.set_admin_qr(if qr_text.is_empty() { Image::default() } else { qr::qr_image(&qr_text) });
                g.set_admin_url(if url.is_empty() { "No network yet".into() } else { url.as_str().into() });
                g.set_admin_hint(hint.as_str().into());
            }
        });
    }

    fn apply_status(&mut self, s: &MachineStatus) {
        let detail = match s.link {
            _ if !s.busy.as_deref().unwrap_or("").is_empty() => s.busy.clone().unwrap_or_default(),
            Link::Connected => format!(
                "Connected · {}{}",
                s.machine_state.as_deref().unwrap_or("ready").replace('_', " "),
                s.address.as_deref().map(|a| format!(" · {a}")).unwrap_or_default()
            ),
            Link::Connecting => "Connecting…".into(),
            Link::Disconnected => match (&s.error, s.reconnect_in_s) {
                (Some(e), Some(n)) => format!("{e} Retrying in {n} s."),
                (Some(e), None) => e.clone(),
                (None, Some(n)) => format!("Offline · retrying in {n} s"),
                (None, None) => "Disconnected".into(),
            },
        };
        let brew = match &s.brew {
            Some(b) if !self.dismissed => {
                self.brew_target = b.target_g;
                BrewInfo {
                    active: true,
                    finished: b.finished,
                    success: b.outcome.as_deref() == Some("completed"),
                    error: b.error,
                    remote: b.remote_start,
                    stage: b.stage.as_str().into(),
                    message: b.message.as_str().into(),
                    recipe: b.recipe_name.as_str().into(),
                    water: b.water_g,
                    coffee: b.coffee_g,
                    target: b.target_g,
                    progress: b.progress,
                    elapsed: format::mmss(b.elapsed_s).into(),
                    stage_index: b.stage_index as i32,
                    stages: ModelRc::new(VecModel::from(
                        b.stages.iter().map(|x| SharedString::from(x.as_str())).collect::<Vec<_>>(),
                    )),
                    outcome: b.outcome.clone().unwrap_or_default().into(),
                }
            }
            _ => BrewInfo::default(),
        };
        let scan: Vec<ScanRow> = s
            .scan_results
            .iter()
            .map(|d| ScanRow {
                address: d.address.as_str().into(),
                name: d.name.clone().unwrap_or_else(|| d.address.clone()).into(),
                signal: d.rssi.map_or_else(String::new, |r| format!("{r} dBm")).into(),
            })
            .collect();
        self.with(|ui| {
            let g = ui.global::<AppState>();
            g.set_link(link_state(s.link));
            g.set_machine_name(s.name.clone().unwrap_or_else(|| "xBloom".into()).into());
            g.set_machine_detail(detail.into());
            g.set_scanning(s.scanning);
            g.set_scan_results(ModelRc::new(VecModel::from(scan)));
            g.set_machine_busy(s.busy.clone().unwrap_or_default().into());
            g.set_brew(brew);
        });
    }

    fn refresh_chart(&mut self) {
        if !self.chart_dirty {
            return;
        }
        self.chart_dirty = false;
        let scale = ChartScale::fit(self.brew_target.max(1.0), self.brew_expected_s, &self.samples);
        let (water, coffee) = format::chart_paths(&self.samples, scale);
        let ty = scale.target_y(self.brew_target);
        self.with(|ui| {
            let g = ui.global::<AppState>();
            g.set_chart_water(water.into());
            g.set_chart_coffee(coffee.into());
            g.set_chart_target_y(ty);
        });
    }

    fn tick_clock(&self) {
        let now = clock::now();
        let h24 = self.app.settings().appearance.clock_24h;
        self.with(|ui| {
            let g = ui.global::<AppState>();
            let t = clock::time_text(now, h24);
            if g.get_time().as_str() != t {
                g.set_time(t.into());
                g.set_date(clock::date_text(now).into());
                g.set_greeting(clock::greeting(now).into());
            }
        });
    }

    // ── events from the app ───────────────────────────────────────────────────────────

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Status(s) => self.apply_status(&s),
            Event::Sample(s) => {
                self.samples.push(s);
                self.chart_dirty = true;
            }
            Event::BrewStarted => {
                self.samples.clear();
                self.dismissed = false;
                self.chart_dirty = true;
                self.last_activity = Instant::now();
            }
            Event::BrewFinished { outcome, .. } => {
                self.chart_dirty = true;
                let page = self.weak.upgrade().map(|ui| ui.global::<AppState>().get_page());
                if outcome == "completed" && page != Some(Page::Brewing) {
                    self.toast("Coffee ready — enjoy!", "success");
                }
                // Show the result even if the screensaver kicked in.
                self.wake();
            }
            Event::Changed(Data::Settings) => {
                let s = self.app.settings();
                self.apply_settings(&s);
            }
            Event::Changed(Data::Recipes) => self.load_recipes(),
            Event::Changed(Data::History) => self.load_history(),
            Event::Changed(Data::Media) => self.load_media(),
            Event::Changed(Data::Presets) => self.load_presets(),
            Event::LockNow => self.lock(),
            Event::Toast { message, kind } => {
                let k = match kind {
                    ToastKind::Error => "error",
                    ToastKind::Warning => "warning",
                    ToastKind::Success => "success",
                    ToastKind::Info => "info",
                };
                self.toast(&message, k);
            }
        }
    }

    fn toast(&mut self, message: &str, kind: &str) {
        self.with(|ui| {
            ui.global::<AppState>().set_toast(ToastInfo { message: message.into(), kind: kind.into(), visible: true });
        });
        let weak = self.weak.clone();
        self.toast_timer.start(TimerMode::SingleShot, Duration::from_millis(3200), move || {
            if let Some(ui) = weak.upgrade() {
                let g = ui.global::<AppState>();
                let mut t = g.get_toast();
                t.visible = false;
                g.set_toast(t);
            }
        });
    }

    // ── idle: screensaver, dim, sleep, auto-lock ──────────────────────────────────────

    fn idle_tick(&mut self) {
        self.tick_clock();
        // Addresses change as Wi-Fi comes and goes (and the ESP32's hotspot with it).
        if self.last_device_refresh.elapsed() > Duration::from_secs(5) {
            self.load_device();
            // The automatic theme follows the clock.
            if self.app.settings().appearance.theme == ThemeMode::Auto {
                let s = self.app.settings();
                self.apply_settings(&s);
            }
        }
        let Some(ui) = self.weak.upgrade() else { return };
        let g = ui.global::<AppState>();
        let s = self.app.settings();
        let idle = self.last_activity.elapsed();
        let brewing = g.get_brew().active && !g.get_brew().finished;
        if brewing || g.get_needs_setup() {
            return;
        }
        let shown =
            Shown { locked: g.get_locked(), saver: g.get_saver_active(), asleep: g.get_asleep(), dimmed: self.dimmed };
        match idle_step(&s, idle, shown) {
            IdleStep::Stay => {}
            IdleStep::Lock => {
                drop(g);
                self.lock();
            }
            IdleStep::StartSaver => {
                drop(g);
                self.start_saver(&s);
            }
            IdleStep::Dim => {
                self.dimmed = true;
                self.app.hooks().set_brightness(s.appearance.dim_brightness);
            }
            IdleStep::Sleep => {
                g.set_asleep(true);
                self.player = None;
                self.app.hooks().set_brightness(0);
            }
        }
    }

    fn start_saver(&mut self, s: &Settings) {
        let content = match s.screensaver.mode {
            ScreensaverMode::Video => {
                s.screensaver.video.as_deref().and_then(|v| self.app.media.path(v)).map(SaverContent::Video)
            }
            ScreensaverMode::Slideshow => {
                let imgs = self.app.media.images();
                (!imgs.is_empty())
                    .then(|| SaverContent::Slideshow(imgs, Duration::from_secs(u64::from(s.screensaver.slide_s))))
            }
            _ => None,
        };
        let lock = s.security.lock_mode == LockMode::Screen && s.lock_configured();
        self.with(|ui| {
            let g = ui.global::<AppState>();
            g.set_overlay(Overlay::None);
            g.set_saver_has_frame(false);
            g.set_saver_active(true);
            if lock {
                g.set_locked(true);
            }
        });
        if lock {
            self.pin = PinFlow::UnlockScreen;
            self.reset_pin_pad("", "");
        } else if !matches!(self.pin, PinFlow::None) {
            self.pin = PinFlow::None;
        }
        self.admin_pin = None;
        self.saver_started = Some(Instant::now());
        self.dimmed = true;
        self.app.hooks().set_brightness(s.appearance.dim_brightness);
        if let Some(content) = content {
            let weak = self.weak.clone();
            self.player = Some(SaverPlayer::start(
                content,
                Box::new(move |buf, gate| {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            let g = ui.global::<AppState>();
                            if g.get_saver_active() {
                                g.set_saver_frame(Image::from_rgb8(buf));
                                g.set_saver_has_frame(true);
                            }
                        }
                        gate.done();
                    });
                }),
            ));
        }
    }

    fn wake(&mut self) {
        self.last_activity = Instant::now();
        self.player = None;
        self.saver_started = None;
        let mut restore = self.dimmed;
        self.with(|ui| {
            let g = ui.global::<AppState>();
            if g.get_asleep() {
                g.set_asleep(false);
                restore = true;
            }
            g.set_saver_active(false);
            g.set_saver_has_frame(false);
        });
        if restore {
            self.dimmed = false;
            self.app.hooks().set_brightness(self.app.settings().appearance.brightness);
        }
    }

    fn lock(&mut self) {
        let s = self.app.settings();
        if !s.lock_configured() {
            return;
        }
        self.admin_pin = None;
        self.pin = PinFlow::UnlockScreen;
        self.reset_pin_pad("", "");
        self.with(|ui| {
            let g = ui.global::<AppState>();
            g.set_overlay(Overlay::None);
            g.set_locked(true);
            if g.get_page() == Page::Settings {
                g.set_page(Page::Home);
            }
        });
    }

    // ── PIN pad ───────────────────────────────────────────────────────────────────────

    fn reset_pin_pad(&mut self, title: &str, subtitle: &str) {
        self.pin_buf.clear();
        self.with(|ui| {
            let g = ui.global::<AppState>();
            g.set_pin_length(0);
            g.set_pin_error("".into());
            if !title.is_empty() {
                g.set_pin_title(title.into());
                g.set_pin_subtitle(subtitle.into());
            }
        });
    }

    fn ask_pin(&mut self, flow: PinFlow, title: &str, subtitle: &str) {
        self.pin = flow;
        self.reset_pin_pad(title, subtitle);
        self.with(|ui| ui.global::<AppState>().set_overlay(Overlay::Pin));
    }

    fn pin_error(&mut self, msg: &str) {
        self.pin_buf.clear();
        let m = msg.to_string();
        self.with(|ui| {
            let g = ui.global::<AppState>();
            g.set_pin_error(m.into());
            g.set_pin_length(0);
        });
    }

    fn pin_key(&mut self, key: &str) {
        self.last_activity = Instant::now();
        match key {
            "del" => {
                self.pin_buf.pop();
            }
            "ok" => return self.pin_submit(),
            d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() && self.pin_buf.len() < 8 => self.pin_buf.push_str(d),
            _ => {}
        }
        let n = self.pin_buf.len() as i32;
        self.with(|ui| {
            let g = ui.global::<AppState>();
            g.set_pin_length(n);
            g.set_pin_error("".into());
        });
    }

    fn check(&self, purpose: PinPurpose, pin: &str) -> Result<(), String> {
        match self.app.check_pin(Channel::Device, purpose, pin) {
            Attempt::Ok => Ok(()),
            other => attempt_ok(other),
        }
    }

    fn pin_submit(&mut self) {
        let pin = std::mem::take(&mut self.pin_buf);
        if !valid_pin(&pin) {
            return self.pin_error("Use 4 to 8 digits");
        }
        let flow = std::mem::take(&mut self.pin);
        let close = |me: &mut Ui| {
            me.reset_pin_pad("", "");
            me.with(|ui| ui.global::<AppState>().set_overlay(Overlay::None));
        };
        match flow {
            PinFlow::None => close(self),
            PinFlow::UnlockScreen => match self.check(PinPurpose::Unlock, &pin) {
                Ok(()) => {
                    self.reset_pin_pad("", "");
                    self.last_activity = Instant::now();
                    self.with(|ui| ui.global::<AppState>().set_locked(false));
                }
                Err(e) => {
                    self.pin = PinFlow::UnlockScreen;
                    self.pin_error(&e);
                }
            },
            PinFlow::Settings => match self.check(PinPurpose::Admin, &pin) {
                Ok(()) => {
                    self.admin_pin = Some(pin);
                    close(self);
                    self.with(|ui| ui.global::<AppState>().set_page(Page::Settings));
                }
                Err(e) => {
                    self.pin = PinFlow::Settings;
                    self.pin_error(&e);
                }
            },
            PinFlow::BrewUnlock { start } => match self.check(PinPurpose::Unlock, &pin) {
                Ok(()) => {
                    close(self);
                    self.do_brew(start);
                }
                Err(e) => {
                    self.pin = PinFlow::BrewUnlock { start };
                    self.pin_error(&e);
                }
            },
            PinFlow::ChangeAdminCurrent => match self.check(PinPurpose::Admin, &pin) {
                Ok(()) => self.ask_pin(PinFlow::ChangeAdminNew { current: pin }, "New admin PIN", "4 to 8 digits"),
                Err(e) => {
                    self.pin = PinFlow::ChangeAdminCurrent;
                    self.pin_error(&e);
                }
            },
            PinFlow::ChangeAdminNew { current } => self.ask_pin(
                PinFlow::ChangeAdminConfirm { current, new: pin },
                "Confirm the new PIN",
                "Type it once more",
            ),
            PinFlow::ChangeAdminConfirm { current, new } => {
                if pin != new {
                    self.ask_pin(PinFlow::ChangeAdminNew { current }, "New admin PIN", "4 to 8 digits");
                    return self.pin_error("The PINs didn't match — try again");
                }
                match self.app.change_admin_pin(Channel::Device, &current, &new) {
                    Ok(()) => {
                        self.admin_pin = Some(new);
                        close(self);
                        self.toast("Admin PIN changed", "success");
                    }
                    Err(e) => {
                        close(self);
                        self.toast(&e, "error");
                    }
                }
            }
            PinFlow::UserNew => {
                self.ask_pin(PinFlow::UserConfirm { new: pin }, "Confirm the lock PIN", "Type it once more")
            }
            PinFlow::UserConfirm { new } => {
                if pin != new {
                    self.ask_pin(PinFlow::UserNew, "New lock PIN", "Used to unlock the screen and to brew");
                    return self.pin_error("The PINs didn't match — try again");
                }
                let admin = self.admin_pin.clone().unwrap_or_default();
                let res = self.app.set_user_pin(Channel::Device, &admin, Some(&new));
                close(self);
                match res {
                    Ok(()) => self.toast("Lock PIN set", "success"),
                    Err(e) => self.toast(&e, "error"),
                }
            }
            PinFlow::SetupNew => {
                self.pin = PinFlow::SetupConfirm { new: pin };
                self.reset_pin_pad("Confirm your PIN", "Type the same PIN again.");
            }
            PinFlow::SetupConfirm { new } => {
                if pin != new {
                    self.pin = PinFlow::SetupNew;
                    self.reset_pin_pad(
                        "Create an admin PIN",
                        "4 to 8 digits. It protects the settings and the admin panel.",
                    );
                    return self.pin_error("The PINs didn't match — try again");
                }
                match self.app.setup_admin_pin(&new) {
                    Ok(()) | Err(_) => {
                        self.reset_pin_pad("", "");
                        self.with(|ui| ui.global::<AppState>().set_setup_step(2));
                        self.app.scan();
                    }
                }
            }
        }
    }

    // ── actions ───────────────────────────────────────────────────────────────────────

    fn navigate(&mut self, page: Page) {
        if page == Page::Settings {
            self.load_device();
        }
        let Some(ui) = self.weak.upgrade() else { return };
        let g = ui.global::<AppState>();
        if page != Page::Settings {
            self.admin_pin = None;
            g.set_page(page);
            return;
        }
        if g.get_page() == Page::Settings {
            return;
        }
        if self.app.needs_setup() || self.admin_pin.is_some() {
            g.set_page(Page::Settings);
            return;
        }
        drop(g);
        self.ask_pin(PinFlow::Settings, "Admin PIN", "Settings are protected");
    }

    fn open_recipe(&mut self, id: &str) {
        let Some(ui) = self.weak.upgrade() else { return };
        let g = ui.global::<AppState>();
        let Some(card) = g.get_recipes().iter().find(|c| c.id == id) else { return };
        let r = self.app.recipes.lock().unwrap().get(id).map(|s| s.recipe);
        self.brew_expected_s = r.as_ref().map_or(180.0, format::estimate_seconds);
        g.set_brew_dose(card.dose);
        g.set_current(card);
        g.set_page(Page::Recipe);
    }

    fn start_brew(&mut self, start: bool) {
        if self.app.brew_needs_pin() {
            return self.ask_pin(
                PinFlow::BrewUnlock { start },
                "Enter PIN to brew",
                "Brewing is locked on this screen",
            );
        }
        self.do_brew(start);
    }

    fn do_brew(&mut self, start: bool) {
        let Some(ui) = self.weak.upgrade() else { return };
        let g = ui.global::<AppState>();
        let id = g.get_current().id.to_string();
        let dose = g.get_brew_dose();
        match self.app.brew(&id, start, Some(dose)) {
            Ok(()) => {
                self.samples.clear();
                self.chart_dirty = true;
                self.dismissed = false;
                g.set_page(Page::Brewing);
            }
            Err(e) => {
                drop(g);
                self.toast(&e, "error");
            }
        }
    }

    fn brew_dismiss(&mut self) {
        self.dismissed = true;
        self.with(|ui| {
            let g = ui.global::<AppState>();
            g.set_brew(BrewInfo::default());
            g.set_page(Page::Home);
        });
    }

    fn assign_slot(&mut self, slot: usize, id: &str) {
        let mut slots = self.app.presets.lock().unwrap().presets.slots.clone();
        if let Some(s) = slots.get_mut(slot) {
            *s = Some(id.to_string());
        }
        if let Err(e) = self.app.set_presets(slots) {
            self.toast(&e, "error");
        }
    }

    fn setup_start(&mut self) {
        if self.app.needs_setup() {
            self.pin = PinFlow::SetupNew;
            self.reset_pin_pad("Create an admin PIN", "4 to 8 digits. It protects the settings and the admin panel.");
            self.with(|ui| ui.global::<AppState>().set_setup_step(1));
        } else {
            self.with(|ui| ui.global::<AppState>().set_setup_step(2));
            self.app.scan();
        }
    }
}

macro_rules! on {
    ($ui:expr, $st:expr, $method:ident, |$me:ident $(, $arg:ident : $ty:ty)*| $body:expr) => {{
        let st = Rc::clone(&$st);
        $ui.global::<AppState>().$method(move |$($arg: $ty),*| {
            let mut guard = st.borrow_mut();
            let $me: &mut Ui = &mut guard;
            $body
        });
    }};
}

/// What the screen shows right now, for the idle timer.
#[derive(Debug, Clone, Copy, Default)]
struct Shown {
    locked: bool,
    saver: bool,
    asleep: bool,
    dimmed: bool,
}

/// The idle timer's next step (one per tick).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleStep {
    Stay,
    Lock,
    StartSaver,
    /// No screensaver: just dim the backlight.
    Dim,
    Sleep,
}

/// After `idle` without a touch: lock (screen lock), then the screensaver — or, with the
/// screensaver off, a dimmed screen — and after `sleep_after_s` more, the backlight off.
fn idle_step(s: &Settings, idle: Duration, shown: Shown) -> IdleStep {
    let idle_s = Duration::from_secs(u64::from(s.screensaver.idle_s));
    let lock_after = Duration::from_secs(u64::from(s.security.auto_lock_s));
    if s.security.lock_mode == LockMode::Screen && s.lock_configured() && !shown.locked && idle >= lock_after {
        return IdleStep::Lock;
    }
    if !shown.saver && idle >= idle_s {
        if s.screensaver.mode != ScreensaverMode::Off {
            return IdleStep::StartSaver;
        }
        if !shown.dimmed {
            return IdleStep::Dim;
        }
    }
    let sleep_after = s.screensaver.sleep_after_s;
    if sleep_after > 0 && !shown.asleep && idle >= idle_s + Duration::from_secs(u64::from(sleep_after)) {
        return IdleStep::Sleep;
    }
    IdleStep::Stay
}

/// Smallest logical screen the layouts are designed for: landscape and portrait.
const MIN_LANDSCAPE: (f32, f32) = (480.0, 272.0);
const MIN_PORTRAIT: (f32, f32) = (320.0, 480.0);

/// The largest scale factor that keeps a `width` × `height` (physical px) screen at least the
/// smallest layout. Below 1 on panels smaller than that, which then render scaled down.
pub fn max_scale(width: u32, height: u32) -> f32 {
    let (w, h) = if height > width { MIN_PORTRAIT } else { MIN_LANDSCAPE };
    (width as f32 / w).min(height as f32 / h)
}

/// Apply the interface-size setting: the window's scale factor becomes `percent` of the
/// platform's own (`SLINT_SCALE_FACTOR`, else 1), capped so the layout stays at least its
/// minimum size. Returns the largest setting this panel allows, in whole 10 % steps.
pub fn apply_interface_size(ui: &AppWindow, percent: u16) -> i32 {
    let window = ui.window();
    let size = window.size();
    if size.width == 0 || size.height == 0 {
        return 200;
    }
    let base: f32 = std::env::var("SLINT_SCALE_FACTOR").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let fit = max_scale(size.width, size.height);
    let scale = (base * f32::from(percent) / 100.0).min(fit);
    if (window.scale_factor() - scale).abs() > 0.001 {
        use slint::platform::WindowEvent;
        window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: scale });
        // Same pixels, new logical size (Slint would otherwise keep the logical size and
        // outgrow the frame buffer).
        window.dispatch_event(WindowEvent::Resized { size: size.to_logical(scale) });
    }
    ((fit / base * 10.0).floor() as i32 * 10).clamp(70, 200)
}

/// Bind `ui` to `app`. Call once on the UI thread, before running the event loop.
pub fn bind(ui: &AppWindow, app: Arc<App>) {
    clock::reload_zone();
    let st = Rc::new(RefCell::new(Ui {
        app: Arc::clone(&app),
        weak: ui.as_weak(),
        pin: PinFlow::None,
        pin_buf: String::new(),
        admin_pin: None,
        last_activity: Instant::now(),
        saver_started: None,
        player: None,
        samples: Vec::new(),
        chart_dirty: false,
        brew_target: 240.0,
        brew_expected_s: 180.0,
        dismissed: false,
        toast_timer: Timer::default(),
        last_device_refresh: Instant::now(),
        dimmed: false,
    }));
    UI.with(|u| *u.borrow_mut() = Some(Rc::clone(&st)));
    st.borrow_mut().load_all();
    // Again once the window is up: showing it can reset the scale factor.
    {
        let st = Rc::clone(&st);
        Timer::single_shot(Duration::ZERO, move || {
            let me = st.borrow();
            me.apply_scale(me.app.settings().appearance.ui_scale);
        });
    }
    app.hooks().set_brightness(app.settings().appearance.brightness);
    // A configured screen lock starts locked after a reboot.
    {
        let s = app.settings();
        if s.setup_complete && s.security.lock_mode == LockMode::Screen && s.lock_configured() {
            st.borrow_mut().lock();
        }
    }

    on!(ui, st, on_activity, |me| {
        me.last_activity = Instant::now();
        // With the screensaver off the idle screen is only dimmed: a touch brings it back.
        if me.dimmed {
            me.wake();
        }
    });
    on!(ui, st, on_wake, |me| me.wake());
    on!(ui, st, on_navigate, |me, p: Page| me.navigate(p));
    on!(ui, st, on_open_recipe, |me, id: SharedString| me.open_recipe(&id));
    on!(ui, st, on_toggle_favorite, |me, id: SharedString| me.app.toggle_favorite(&id));
    on!(ui, st, on_start_brew, |me, start: bool| me.start_brew(start));
    on!(ui, st, on_cancel_brew, |me| me.app.cancel_brew());
    on!(ui, st, on_brew_dismiss, |me| me.brew_dismiss());
    on!(ui, st, on_pin_key, |me, k: SharedString| me.pin_key(&k));
    on!(ui, st, on_pin_cancel, |me| {
        if !matches!(me.pin, PinFlow::UnlockScreen | PinFlow::SetupNew | PinFlow::SetupConfirm { .. }) {
            me.pin = PinFlow::None;
            me.reset_pin_pad("", "");
            me.with(|ui| ui.global::<AppState>().set_overlay(Overlay::None));
        }
    });
    on!(ui, st, on_lock_now, |me| me.lock());
    on!(ui, st, on_connect, |me| me.app.connect(None));
    on!(ui, st, on_disconnect, |me| me.app.disconnect());
    on!(ui, st, on_scan, |me| me.app.scan());
    on!(ui, st, on_pair, |me, a: SharedString| me.app.connect(Some(a.to_string())));
    on!(ui, st, on_forget, |me| me.app.forget_machine());
    on!(ui, st, on_set_theme, |me, i: i32| {
        me.app.update_settings(|s| {
            s.appearance.theme = [ThemeMode::Light, ThemeMode::Dark, ThemeMode::Auto][i.clamp(0, 2) as usize]
        });
    });
    on!(ui, st, on_set_accent, |me, c: slint::Color| {
        me.app.update_settings(|s| s.appearance.accent = format::to_hex(c));
    });
    on!(ui, st, on_set_brightness, |me, v: i32| {
        me.app.update_settings(|s| s.appearance.brightness = v.clamp(10, 100) as u8);
    });
    on!(ui, st, on_set_clock_24h, |me, v: bool| {
        me.app.update_settings(|s| s.appearance.clock_24h = v);
        me.load_history();
    });
    on!(ui, st, on_set_ui_scale, |me, v: i32| {
        me.app.update_settings(|s| s.appearance.ui_scale = v.clamp(70, 200) as u16);
    });
    on!(ui, st, on_set_rotation, |me, i: i32| {
        let degrees = (i.clamp(0, 3) * 90) as u16;
        if me.app.settings().appearance.rotation != degrees {
            // The platform restarts the app to apply it.
            me.toast("Rotating the screen…", "info");
            me.app.update_settings(|s| s.appearance.rotation = degrees);
        }
    });
    on!(ui, st, on_set_saver_mode, |me, i: i32| {
        let mode = [ScreensaverMode::Off, ScreensaverMode::Clock, ScreensaverMode::Video, ScreensaverMode::Slideshow]
            [i.clamp(0, 3) as usize];
        let first_video =
            me.app.media.list().into_iter().find(|m| m.kind == xbloom_app::media::MediaKind::Video).map(|m| m.name);
        me.app.update_settings(|s| {
            s.screensaver.mode = mode;
            if mode == ScreensaverMode::Video && s.screensaver.video.is_none() {
                s.screensaver.video = first_video;
            }
        });
    });
    on!(ui, st, on_set_saver_idle, |me, m: i32| {
        me.app.update_settings(|s| s.screensaver.idle_s = (m.max(1) as u32) * 60);
    });
    on!(ui, st, on_set_saver_video, |me, name: SharedString| {
        me.app.update_settings(|s| {
            s.screensaver.video = Some(name.to_string());
            s.screensaver.mode = ScreensaverMode::Video;
        });
    });
    on!(ui, st, on_set_saver_clock, |me, v: bool| {
        me.app.update_settings(|s| s.screensaver.clock_overlay = v);
    });
    on!(ui, st, on_set_lock_mode, |me, i: i32| {
        me.app.update_settings(|s| {
            s.security.lock_mode = [LockMode::Off, LockMode::Brew, LockMode::Screen][i.clamp(0, 2) as usize]
        });
    });
    on!(ui, st, on_set_auto_lock, |me, m: i32| {
        me.app.update_settings(|s| s.security.auto_lock_s = (m.max(1) as u32) * 60);
    });
    on!(ui, st, on_set_remote_start, |me, v: bool| {
        me.app.update_settings(|s| s.security.allow_remote_start = v);
    });
    on!(ui, st, on_set_hold_ms, |me, v: i32| {
        me.app.update_settings(|s| s.security.hold_to_brew_ms = v.clamp(500, 3000) as u32);
    });
    on!(ui, st, on_set_auto_connect, |me, v: bool| {
        me.app.update_settings(|s| s.machine.auto_connect = v);
    });
    on!(ui, st, on_set_simulator, |me, v: bool| {
        me.app.update_settings(|s| s.machine.simulator = v);
    });
    on!(ui, st, on_change_admin_pin, |me| me.ask_pin(
        PinFlow::ChangeAdminCurrent,
        "Current admin PIN",
        "To change it, enter it first"
    ));
    on!(ui, st, on_set_user_pin, |me| me.ask_pin(
        PinFlow::UserNew,
        "New lock PIN",
        "Used to unlock the screen and to brew"
    ));
    on!(ui, st, on_clear_user_pin, |me| {
        let admin = me.admin_pin.clone().unwrap_or_default();
        match me.app.set_user_pin(Channel::Device, &admin, None) {
            Ok(()) => me.toast("Lock PIN removed — the admin PIN unlocks", "success"),
            Err(e) => me.toast(&e, "error"),
        }
    });
    on!(ui, st, on_assign_slot, |me, slot: i32, id: SharedString| me.assign_slot(slot.max(0) as usize, &id));
    on!(ui, st, on_set_slot_scale, |me, slot: i32, v: bool| {
        me.app.update_settings(|s| {
            if let Some(x) = s.machine.preset_scale.get_mut(slot.max(0) as usize) {
                *x = v;
            }
        });
        me.load_presets();
    });
    on!(ui, st, on_push_presets, |me| {
        if let Err(e) = me.app.push_presets() {
            me.toast(&e, "error");
        }
    });
    on!(ui, st, on_select_history, |me, i: i32| me.select_history(i));
    on!(ui, st, on_setup_start, |me| me.setup_start());
    on!(ui, st, on_setup_demo, |me| {
        me.app.update_settings(|s| s.machine.simulator = true);
        me.with(|ui| ui.global::<AppState>().set_setup_step(3));
    });
    on!(ui, st, on_setup_finish, |me| {
        me.app.update_settings(|s| s.setup_complete = true);
        me.with(|ui| ui.global::<AppState>().set_page(Page::Home));
    });
    on!(ui, st, on_restart, |me| me.app.hooks().restart());

    // App events → the UI thread.
    app.subscribe(move |ev| {
        let ev = ev.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let st = UI.with(|u| u.borrow().clone());
            if let Some(st) = st {
                st.borrow_mut().on_event(ev);
            }
        });
    });

    // Timers live as long as the UI (leaked into the thread-local state's lifetime).
    let idle = Timer::default();
    let st_idle = Rc::clone(&st);
    idle.start(TimerMode::Repeated, Duration::from_secs(1), move || st_idle.borrow_mut().idle_tick());
    let chart = Timer::default();
    let st_chart = Rc::clone(&st);
    chart.start(TimerMode::Repeated, Duration::from_millis(250), move || st_chart.borrow_mut().refresh_chart());
    std::mem::forget(idle);
    std::mem::forget(chart);
}

/// Simulate inactivity (screenshots / tests): start the screensaver now.
pub fn force_screensaver() {
    let st = UI.with(|u| u.borrow().clone());
    if let Some(st) = st {
        let s = st.borrow().app.settings();
        st.borrow_mut().start_saver(&s);
    }
}

/// Lock the screen now (screenshots / tests).
pub fn force_lock() {
    let st = UI.with(|u| u.borrow().clone());
    if let Some(st) = st {
        st.borrow_mut().lock();
    }
}

/// Wake the screen (screenshots / tests).
pub fn force_wake() {
    let st = UI.with(|u| u.borrow().clone());
    if let Some(st) = st {
        st.borrow_mut().wake();
    }
}

#[cfg(test)]
mod tests {
    use super::{idle_step, max_scale, IdleStep, Shown};
    use std::time::Duration;
    use xbloom_app::settings::{ScreensaverMode, Settings};

    #[test]
    fn idle_screen_dims_then_sleeps_even_without_a_screensaver() {
        let mut s = Settings::default();
        s.screensaver.idle_s = 60;
        s.screensaver.sleep_after_s = 600;
        let secs = Duration::from_secs;
        for mode in [ScreensaverMode::Off, ScreensaverMode::Clock] {
            s.screensaver.mode = mode;
            let first = if mode == ScreensaverMode::Off { IdleStep::Dim } else { IdleStep::StartSaver };
            assert_eq!(idle_step(&s, secs(30), Shown::default()), IdleStep::Stay);
            assert_eq!(idle_step(&s, secs(61), Shown::default()), first);
            // Dimmed, or showing the screensaver: then the backlight goes off.
            let shown = if mode == ScreensaverMode::Off {
                Shown { dimmed: true, ..Shown::default() }
            } else {
                Shown { saver: true, ..Shown::default() }
            };
            assert_eq!(idle_step(&s, secs(300), shown), IdleStep::Stay);
            assert_eq!(idle_step(&s, secs(661), shown), IdleStep::Sleep, "{mode:?}");
            assert_eq!(idle_step(&s, secs(700), Shown { asleep: true, ..shown }), IdleStep::Stay);
        }
    }

    #[test]
    fn interface_size_never_shrinks_the_layout_below_its_minimum() {
        // 800 × 480 can go to 166 % before its width drops under 480.
        assert!((max_scale(800, 480) - 800.0 / 480.0).abs() < 1e-6);
        // A 1920 × 1080 panel has room to spare; 720 × 1280 portrait is limited by its width.
        assert!(max_scale(1920, 1080) > 3.9);
        assert!((max_scale(720, 1280) - 720.0 / 320.0).abs() < 1e-6);
        // A panel smaller than the minimum renders scaled down to fit.
        assert!(max_scale(320, 240) < 1.0);
    }
}
