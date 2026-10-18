//! The machine controller: one thread that owns the BLE conversation.
//!
//! Everything that talks to the machine — connect, brew, cancel, program the dial presets —
//! is a [`Command`] sent to this thread, which runs them one at a time (the machine accepts a
//! single link and one conversation anyway). It holds the link open between brews (fast
//! brews, the machine shows "connected"), reconnects with backoff when it drops, and reports
//! everything through [`MachineStatus`] snapshots and [`Event`]s on the app's bus.
//!
//! 🛑 A brew command carries an explicit `start` flag. With `start: false` the controller only
//! loads (the machine arms; the human approves on it). It calls [`Machine::start`] only when the
//! caller explicitly asked for a remote start — never as a side effect of loading.

use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use xbloom_core::brew::{BrewMonitor, BrewOutcome, BrewPhase, BrewSignal, Sample};
use xbloom_core::driver::{Discovered, DriverError, Machine, Timings, Transport, TransportError};
use xbloom_core::sim::{SimConfig, SimFault, SimTransport, SIM_ADDRESS, SIM_NAME};
use xbloom_core::{MachineState, Recipe};

use crate::app::{App, Data, Event, ToastKind};
use crate::history::HistoryEntry;
use crate::util::now_unix;

/// Work for the controller thread.
#[derive(Debug)]
pub enum Command {
    /// Connect to `address`, or the saved machine.
    Connect {
        /// Address to pair with (saved on success); `None` = the saved one.
        address: Option<String>,
    },
    /// Drop the link and stay disconnected (e.g. to hand the machine to the phone app).
    Disconnect,
    /// Disconnect and forget the saved machine.
    Forget,
    /// Scan for machines (results land in [`MachineStatus::scan_results`]).
    Scan,
    /// Load a recipe and — only if `start` — start it remotely.
    Brew {
        /// The recipe (already validated; possibly dose-adjusted).
        recipe: Box<Recipe>,
        /// Its library id, for history.
        recipe_id: Option<String>,
        /// 🔥 Send commit + start after loading. `false` = load only (approve on the machine).
        start: bool,
        /// What the run is for.
        kind: BrewKind,
    },
    /// Cancel the current brew / disarm.
    Cancel,
    /// Program dial presets A/B/C.
    SavePresets {
        /// The three recipes, A/B/C.
        recipes: Box<[Recipe; 3]>,
        /// Scale flag per slot.
        scale: [bool; 3],
    },
    /// Switch between the simulator and real hardware.
    SetSimulator(bool),
    /// Demo only: make the simulated machine refuse the next brew.
    SimFault(Option<SimFault>),
    /// Stop the thread.
    Shutdown,
}

/// What a run of the machine is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BrewKind {
    /// A library recipe, started from the screen (or approved on the machine).
    #[default]
    Recipe,
    /// The rinse-filter button: hot water through the empty filter. Not a brew, so it isn't
    /// recorded in history.
    Rinse,
    /// A library recipe started by an armed timer.
    Scheduled,
}

/// Link state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    /// No link.
    #[default]
    Disconnected,
    /// Connecting.
    Connecting,
    /// Connected; session open.
    Connected,
}

/// A live (or just-finished) brew, for the screen and the admin dashboard.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BrewStatus {
    /// Library id.
    pub recipe_id: Option<String>,
    /// Recipe name.
    pub recipe_name: String,
    /// Started remotely from the screen.
    pub remote_start: bool,
    /// What the run is for.
    pub kind: BrewKind,
    /// `connecting`, `loading`, `starting`, `armed`, `awaiting_confirm`, `grinding`,
    /// `brewing`, `drawdown` or `finished`.
    pub stage: String,
    /// Human message for the stage / outcome.
    pub message: String,
    /// Current pour index.
    pub stage_index: usize,
    /// The pours' labels.
    pub stages: Vec<String>,
    /// Seconds since the brew began.
    pub elapsed_s: f32,
    /// Water dispensed, g.
    pub water_g: f32,
    /// Coffee in the cup, g.
    pub coffee_g: f32,
    /// Target water, g.
    pub target_g: f32,
    /// 0–1.
    pub progress: f32,
    /// Outcome id once finished.
    pub outcome: Option<String>,
    /// Finished?
    pub finished: bool,
    /// Ended in an error (connection / load failure) rather than a brew outcome.
    pub error: bool,
}

/// Everything about the machine, as a snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct MachineStatus {
    /// Link state.
    pub link: Link,
    /// Connected / saved machine address.
    pub address: Option<String>,
    /// Its name.
    pub name: Option<String>,
    /// Last machine state name (`idle`, `armed`, …).
    pub machine_state: Option<String>,
    /// Driving the simulator.
    pub simulator: bool,
    /// A long operation in progress ("Programming dial presets…").
    pub busy: Option<String>,
    /// The last connection error.
    pub error: Option<String>,
    /// A scan is running.
    pub scanning: bool,
    /// Machines found by the last scan.
    pub scan_results: Vec<Discovered>,
    /// The current or last brew.
    pub brew: Option<BrewStatus>,
    /// Seconds until the next reconnect attempt.
    pub reconnect_in_s: Option<u32>,
}

fn phase_stage(p: BrewPhase) -> &'static str {
    match p {
        BrewPhase::Armed => "armed",
        BrewPhase::AwaitingConfirm => "awaiting_confirm",
        BrewPhase::Starting => "grinding",
        BrewPhase::Brewing => "brewing",
        BrewPhase::Drawdown => "drawdown",
        BrewPhase::Finished => "finished",
    }
}

/// Explain a driver failure to a person standing at the machine.
pub fn explain(e: &DriverError) -> String {
    match e {
        DriverError::Timeout("armed after load") => {
            "The machine didn't accept the recipe. Make sure the xBloom phone app is closed (only one \
             Bluetooth connection is allowed) and the machine is on its home screen, then try again."
                .into()
        }
        DriverError::Timeout("slots saved") => {
            "The machine didn't confirm the presets (it may show RETRY). Close the phone app and try again.".into()
        }
        DriverError::Transport(TransportError::NotFound(_)) => {
            "Machine not found. Is it switched on and in range?".into()
        }
        DriverError::Transport(TransportError::Disconnected) => "The machine disconnected.".into(),
        other => other.to_string(),
    }
}

pub(crate) struct Controller {
    app: Arc<App>,
    rx: Receiver<Command>,
    machine: Option<Machine<Box<dyn Transport>>>,
    spare: Option<Box<dyn Transport>>,
    sim_ctl: Option<Arc<Mutex<SimConfig>>>,
    sim_cfg: SimConfig,
    timings: Timings,
    epoch: Instant,
    manual_disconnect: bool,
    next_retry: Option<Instant>,
    backoff: Duration,
}

const MIN_BACKOFF: Duration = Duration::from_secs(3);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

impl Controller {
    pub(crate) fn new(
        app: Arc<App>,
        rx: Receiver<Command>,
        real: Box<dyn Transport>,
        sim_cfg: SimConfig,
        timings: Timings,
    ) -> Self {
        Self {
            app,
            rx,
            machine: None,
            spare: Some(real),
            sim_ctl: None,
            sim_cfg,
            timings,
            epoch: Instant::now(),
            manual_disconnect: false,
            next_retry: None,
            backoff: MIN_BACKOFF,
        }
    }

    fn status(&self, f: impl FnOnce(&mut MachineStatus)) {
        let snapshot = {
            let mut s = self.app.status.lock().unwrap();
            f(&mut s);
            s.clone()
        };
        self.app.emit(Event::Status(snapshot));
    }

    /// Simulated seconds since the controller started (the sim may run faster than realtime).
    fn now(&self) -> f32 {
        let scale = if self.using_sim() { self.sim_cfg.speed } else { 1.0 };
        self.epoch.elapsed().as_secs_f32() * scale
    }

    fn using_sim(&self) -> bool {
        self.sim_ctl.is_some()
    }

    fn machine(&mut self) -> &mut Machine<Box<dyn Transport>> {
        if self.machine.is_none() {
            let simulator = self.app.settings().machine.simulator;
            self.install_transport(simulator);
        }
        self.machine.as_mut().expect("transport installed")
    }

    /// Put the real transport or a fresh simulator under the driver.
    fn install_transport(&mut self, simulator: bool) {
        if let Some(m) = self.machine.take() {
            let t = m.into_transport();
            if self.sim_ctl.is_none() {
                self.spare = Some(t); // keep the real transport
            }
        }
        let transport: Box<dyn Transport> = if simulator {
            let sim = SimTransport::new(self.sim_cfg.clone());
            self.sim_ctl = Some(sim.control());
            Box::new(sim)
        } else {
            self.sim_ctl = None;
            match self.spare.take() {
                Some(t) => t,
                None => {
                    log::error!("controller: no hardware transport available — using the simulator");
                    let sim = SimTransport::new(self.sim_cfg.clone());
                    self.sim_ctl = Some(sim.control());
                    Box::new(sim)
                }
            }
        };
        let timings = if self.sim_ctl.is_some() { Timings::scaled(0.25) } else { self.timings };
        self.machine = Some(Machine::new(transport, timings));
        let sim = self.using_sim();
        self.status(|s| {
            *s = MachineStatus { simulator: sim, ..MachineStatus::default() };
        });
    }

    pub(crate) fn run(mut self) {
        let settings = self.app.settings();
        self.install_transport(settings.machine.simulator);
        if settings.machine.auto_connect && (settings.machine.address.is_some() || settings.machine.simulator) {
            self.connect(None);
        }
        loop {
            // 1. Commands first.
            let connected = self.machine.as_ref().is_some_and(Machine::is_connected);
            let cmd = if connected {
                match self.rx.try_recv() {
                    Ok(c) => Some(c),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return,
                }
            } else {
                match self.rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            };
            if let Some(cmd) = cmd {
                if !self.handle(cmd) {
                    if let Some(m) = self.machine.as_mut() {
                        m.disconnect();
                    }
                    return;
                }
                continue;
            }
            // 2. Connected and idle: keep the notification queue drained and track state.
            if connected {
                self.idle_poll();
            } else {
                self.maybe_reconnect();
            }
        }
    }

    fn idle_poll(&mut self) {
        let before = self.machine().last_state();
        match self.machine().next_event(Duration::from_millis(250)) {
            Ok(_) => {
                let after = self.machine().last_state();
                if after != before {
                    let name = after.map(MachineState::name);
                    self.status(|s| s.machine_state = name);
                }
            }
            Err(e) => self.link_lost(&e),
        }
    }

    fn link_lost(&mut self, e: &DriverError) {
        log::warn!("controller: link lost: {e}");
        if let Some(m) = self.machine.as_mut() {
            m.disconnect();
        }
        self.schedule_retry();
        self.status(|s| {
            s.link = Link::Disconnected;
            s.machine_state = None;
            s.error = Some("Connection lost — reconnecting…".into());
        });
        self.app.toast("Machine disconnected", ToastKind::Warning);
    }

    fn schedule_retry(&mut self) {
        let settings = self.app.settings();
        let has_target = settings.machine.address.is_some() || self.using_sim();
        if self.manual_disconnect || !settings.machine.auto_connect || !has_target {
            self.next_retry = None;
            return;
        }
        self.next_retry = Some(Instant::now() + self.backoff);
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
    }

    fn maybe_reconnect(&mut self) {
        let Some(at) = self.next_retry else { return };
        let left = at.saturating_duration_since(Instant::now());
        if !left.is_zero() {
            let secs = left.as_secs() as u32 + 1;
            if self.app.status.lock().unwrap().reconnect_in_s != Some(secs) {
                self.status(|s| s.reconnect_in_s = Some(secs));
            }
            return;
        }
        self.next_retry = None;
        self.connect(None);
    }

    /// Returns false to stop the thread.
    fn handle(&mut self, cmd: Command) -> bool {
        match cmd {
            Command::Connect { address } => {
                self.manual_disconnect = false;
                self.connect(address);
            }
            Command::Disconnect => {
                self.manual_disconnect = true;
                self.next_retry = None;
                self.machine().disconnect();
                self.status(|s| {
                    s.link = Link::Disconnected;
                    s.machine_state = None;
                    s.reconnect_in_s = None;
                    s.error = None;
                });
            }
            Command::Forget => {
                self.manual_disconnect = true;
                self.next_retry = None;
                self.machine().disconnect();
                self.app.update_settings(|s| {
                    s.machine.address = None;
                    s.machine.name = None;
                });
                self.status(|s| {
                    s.link = Link::Disconnected;
                    s.address = None;
                    s.name = None;
                    s.machine_state = None;
                    s.reconnect_in_s = None;
                });
            }
            Command::Scan => self.scan(),
            Command::Brew { recipe, recipe_id, start, kind } => self.brew(&recipe, recipe_id, start, kind),
            Command::Cancel => {
                if self.machine().is_connected() {
                    if let Err(e) = self.machine().cancel() {
                        self.link_lost(&e);
                    }
                }
            }
            Command::SavePresets { recipes, scale } => self.save_presets(&recipes, scale),
            Command::SetSimulator(on) => {
                if on != self.using_sim() {
                    self.next_retry = None;
                    self.install_transport(on);
                    if self.app.settings().machine.auto_connect && (on || self.app.settings().machine.address.is_some())
                    {
                        self.connect(None);
                    }
                }
            }
            Command::SimFault(f) => {
                if let Some(ctl) = &self.sim_ctl {
                    ctl.lock().unwrap().fault = f;
                }
            }
            Command::Shutdown => return false,
        }
        true
    }

    fn connect(&mut self, address: Option<String>) -> bool {
        let settings = self.app.settings();
        let sim = self.using_sim();
        let target =
            address.clone().or(if sim { Some(SIM_ADDRESS.to_string()) } else { settings.machine.address.clone() });
        let Some(addr) = target else {
            self.status(|s| s.error = Some("No machine paired yet — scan and pick one.".into()));
            return false;
        };
        if self.machine().is_connected() && self.machine().address() == Some(addr.as_str()) {
            return true;
        }
        self.status(|s| {
            s.link = Link::Connecting;
            s.error = None;
            s.reconnect_in_s = None;
        });
        match self.machine().connect(&addr) {
            Ok(()) => {
                self.backoff = MIN_BACKOFF;
                self.next_retry = None;
                let name = if sim {
                    Some(SIM_NAME.to_string())
                } else {
                    let found = self
                        .app
                        .status
                        .lock()
                        .unwrap()
                        .scan_results
                        .iter()
                        .find(|d| d.address == addr)
                        .and_then(|d| d.name.clone());
                    found.or(settings.machine.name.clone())
                };
                if !sim && (settings.machine.address.as_deref() != Some(addr.as_str()) || address.is_some()) {
                    let (a, n) = (addr.clone(), name.clone());
                    self.app.update_settings(move |s| {
                        s.machine.address = Some(a);
                        s.machine.name = n;
                    });
                }
                self.status(|s| {
                    s.link = Link::Connected;
                    s.address = Some(addr.clone());
                    s.name = name;
                    s.error = None;
                });
                log::info!("controller: connected to {addr}");
                true
            }
            Err(e) => {
                log::warn!("controller: connect to {addr} failed: {e}");
                self.machine().disconnect();
                self.schedule_retry();
                let msg = explain(&e);
                self.status(|s| {
                    s.link = Link::Disconnected;
                    s.error = Some(msg);
                });
                false
            }
        }
    }

    fn scan(&mut self) {
        self.status(|s| {
            s.scanning = true;
            s.scan_results.clear();
        });
        let result = self.machine().scan(Duration::from_secs(6));
        match result {
            Ok(found) => {
                let n = found.len();
                self.status(|s| {
                    s.scanning = false;
                    s.scan_results = found;
                });
                log::info!("controller: scan found {n} machine(s)");
            }
            Err(e) => {
                let msg = explain(&e);
                self.status(|s| {
                    s.scanning = false;
                    s.error = Some(msg);
                });
            }
        }
    }

    fn ensure_connected(&mut self) -> Result<(), String> {
        if self.machine().is_connected() || self.connect(None) {
            Ok(())
        } else {
            Err(self.app.status.lock().unwrap().error.clone().unwrap_or_else(|| "Not connected".into()))
        }
    }

    fn save_presets(&mut self, recipes: &[Recipe; 3], scale: [bool; 3]) {
        self.status(|s| s.busy = Some("Programming dial presets…".into()));
        let result = self.ensure_connected().and_then(|()| {
            self.machine().save_slots([&recipes[0], &recipes[1], &recipes[2]], scale).map_err(|e| explain(&e))
        });
        self.status(|s| s.busy = None);
        match result {
            Ok(()) => {
                {
                    let mut p = self.app.presets.lock().unwrap();
                    p.presets.pushed_at = Some(now_unix());
                    p.presets.pushed_slots = p.presets.slots.clone();
                    p.save();
                }
                self.app.emit(Event::Changed(Data::Presets));
                self.app.toast("Dial presets A · B · C saved to the machine", ToastKind::Success);
            }
            Err(e) => self.app.toast(&format!("Couldn't save presets: {e}"), ToastKind::Error),
        }
    }

    fn brew_status(&self, f: impl FnOnce(&mut BrewStatus)) {
        self.status(|s| {
            if let Some(b) = s.brew.as_mut() {
                f(b);
            }
        });
    }

    fn fail_brew(&self, message: String) {
        log::warn!("brew failed: {message}");
        self.brew_status(|b| {
            b.stage = "finished".into();
            b.finished = true;
            b.error = true;
            b.outcome = Some("error".into());
            b.message = message.clone();
        });
        self.app.emit(Event::BrewFinished { outcome: "error".into(), entry: None });
    }

    fn brew(&mut self, recipe: &Recipe, recipe_id: Option<String>, start: bool, kind: BrewKind) {
        let stages: Vec<String> = (0..recipe.pours.len()).map(|i| xbloom_core::brew::stage_label(recipe, i)).collect();
        let target = recipe.dose_g as f32 * recipe.effective_ratio() as f32;
        self.status(|s| {
            s.brew = Some(BrewStatus {
                recipe_id: recipe_id.clone(),
                recipe_name: recipe.name.clone(),
                remote_start: start,
                kind,
                stage: "connecting".into(),
                message: "Connecting to the machine…".into(),
                stage_index: 0,
                stages: stages.clone(),
                elapsed_s: 0.0,
                water_g: 0.0,
                coffee_g: 0.0,
                target_g: target,
                progress: 0.0,
                outcome: None,
                finished: false,
                error: false,
            });
        });
        self.app.emit(Event::BrewStarted);
        if let Err(e) = self.ensure_connected() {
            return self.fail_brew(e);
        }
        self.brew_status(|b| {
            b.stage = "loading".into();
            b.message = "Sending the recipe…".into();
        });
        if let Err(e) = self.machine().load_recipe(recipe) {
            if matches!(e, DriverError::Transport(_)) {
                self.link_lost(&e);
            }
            return self.fail_brew(explain(&e));
        }
        // 🛑 Loading only armed the machine. Start ONLY when the caller explicitly asked.
        let observed = if start {
            self.brew_status(|b| {
                b.stage = "starting".into();
                b.message = "Starting the brew…".into();
            });
            match self.machine().start() {
                Ok(o) => o.observed,
                Err(e) => {
                    if matches!(e, DriverError::Transport(_)) {
                        self.link_lost(&e);
                    }
                    return self.fail_brew(explain(&e));
                }
            }
        } else {
            None
        };
        let t0 = self.now();
        let mut mon = BrewMonitor::new(recipe, t0, start, observed.as_ref());
        let mut finished = observed.as_ref().and_then(|ev| match mon.on_event(ev, t0) {
            Some(BrewSignal::Finished(o)) => Some(o),
            _ => None,
        });
        self.push_progress(&mon, t0);
        let mut last_push = Instant::now();
        let mut samples_sent = 0;
        while finished.is_none() {
            // Commands during a brew: cancel (or shutdown) — everything else waits.
            match self.rx.try_recv() {
                Ok(Command::Cancel) => {
                    let _ = self.machine().cancel();
                    if let Some(BrewSignal::Finished(o)) = mon.cancel(self.now()) {
                        finished = Some(o);
                    }
                    continue;
                }
                Ok(Command::Shutdown) | Err(TryRecvError::Disconnected) => {
                    let _ = self.machine().cancel();
                    finished = Some(BrewOutcome::Cancelled);
                    continue;
                }
                // Demo mode switched mid-brew: applied from the saved setting when the brew ends.
                Ok(Command::SetSimulator(_)) => {}
                Ok(other) => {
                    log::warn!("controller: busy brewing — ignoring {other:?}");
                    self.app.toast("Busy brewing — try again when it's done", ToastKind::Warning);
                }
                Err(TryRecvError::Empty) => {}
            }
            let signal = match self.machine().next_event(Duration::from_millis(100)) {
                Ok(Some(ev)) => {
                    let now = self.now();
                    mon.on_event(&ev, now)
                }
                Ok(None) => {
                    let now = self.now();
                    mon.tick(now)
                }
                Err(e) => {
                    self.link_lost(&e);
                    finished = Some(BrewOutcome::Stopped);
                    break;
                }
            };
            match signal {
                Some(BrewSignal::Finished(o)) => finished = Some(o),
                Some(BrewSignal::Phase(_)) => {
                    self.push_progress(&mon, self.now());
                    last_push = Instant::now();
                }
                None => {}
            }
            for s in &mon.samples()[samples_sent..] {
                self.app.emit(Event::Sample(*s));
            }
            samples_sent = mon.samples().len();
            if last_push.elapsed() >= Duration::from_millis(250) {
                self.push_progress(&mon, self.now());
                last_push = Instant::now();
            }
        }
        let outcome = finished.unwrap_or(BrewOutcome::Stopped);
        if outcome.needs_cancel() && self.machine().is_connected() {
            let _ = self.machine().cancel();
        }
        self.finish_brew(recipe, recipe_id, start, kind, &mon, outcome);
        // Catch the transport up with demo mode, in case it was switched during the brew.
        let simulator = self.app.settings().machine.simulator;
        if simulator != self.using_sim() {
            self.handle(Command::SetSimulator(simulator));
        }
    }

    fn push_progress(&self, mon: &BrewMonitor, now: f32) {
        let idx = mon.stage_index();
        self.brew_status(|b| {
            b.stage = phase_stage(mon.phase()).into();
            b.message = mon.phase().message().into();
            b.stage_index = idx;
            b.elapsed_s = mon.elapsed(now);
            b.water_g = mon.water_g().max(mon.final_water_g());
            b.coffee_g = mon.coffee_g();
            b.progress = mon.progress();
        });
    }

    fn finish_brew(
        &self,
        recipe: &Recipe,
        recipe_id: Option<String>,
        start: bool,
        kind: BrewKind,
        mon: &BrewMonitor,
        outcome: BrewOutcome,
    ) {
        let now = self.now();
        let duration = mon.elapsed(now);
        log::info!("brew '{}' finished: {} after {duration:.0}s", recipe.name, outcome.id());
        self.brew_status(|b| {
            b.stage = "finished".into();
            b.finished = true;
            b.outcome = Some(outcome.id().into());
            b.message = outcome.message().into();
            b.elapsed_s = duration;
            b.water_g = mon.final_water_g();
            b.coffee_g = mon.final_coffee_g();
            b.progress = if outcome == BrewOutcome::Completed { 1.0 } else { mon.progress() };
        });
        // A rinse is water through an empty filter, not a brew: it isn't recorded.
        let entry = (outcome == BrewOutcome::Completed && kind != BrewKind::Rinse).then(|| {
            let max_points = self.app.history.lock().unwrap().max_points;
            let samples: Vec<Sample> = mon.samples().to_vec();
            let e = HistoryEntry::new(
                now_unix(),
                recipe_id.clone(),
                recipe,
                mon.final_water_g(),
                mon.final_coffee_g(),
                duration,
                start,
                outcome.id(),
                &samples,
                max_points,
            );
            self.app.history.lock().unwrap().record(e.clone());
            if let Some(id) = &recipe_id {
                self.app.recipes.lock().unwrap().record_brew(id);
            }
            e
        });
        if entry.is_some() {
            self.app.emit(Event::Changed(Data::History));
        }
        self.app.emit(Event::BrewFinished { outcome: outcome.id().into(), entry });
    }
}
