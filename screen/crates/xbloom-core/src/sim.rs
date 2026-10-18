//! A simulated xBloom Studio that speaks the real wire protocol.
//!
//! [`SimTransport`] implements [`Transport`]: frames written to it are CRC-checked and parsed
//! exactly as the machine would, and it answers with genuine `58 02 07` notification frames
//! (ACKs, status states, the live water/coffee scale streams). So demo mode and the tests
//! exercise the *whole* stack — frame builders, driver pacing, telemetry decoding and the brew
//! monitor — with no machine attached.
//!
//! It models what the reverse-engineering found on hardware: arming on the pours frame (and
//! rejecting a ratio byte that doesn't match the pours), commit → awaiting-confirm → starting,
//! a **silent** grind, pours at the recipe's flow rate with their pauses, a lagging cup weight
//! that plateaus during drawdown, the `0x24` ready beep, idle on cup-lift, the PRO/AUTO mode
//! rule for dial-slot writes and the atomic A/B/C save. A `0x46` sent into a running brew
//! aborts it back to armed, like the real firmware.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::driver::{Discovered, Transport, TransportError};
use crate::protocol::{self, WirePour};
use crate::pyfmt::round_half_even;
use crate::telemetry::{build_notification, COFFEE_TYPE, STATUS_TYPE, WATER_TYPE};

/// The simulated machine's (placeholder) address.
pub const SIM_ADDRESS: &str = "AA:BB:CC:DD:EE:FF";
/// The simulated machine's advertised name.
pub const SIM_NAME: &str = "XBLOOM-SIM";

/// A refusal the simulated machine reports right after commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimFault {
    /// Report "no beans" (`0x0f`).
    NoBeans,
    /// Report "no water" (`0x0c`).
    NoWater,
}

/// Simulator knobs. Shared behind a mutex so a running sim can be adjusted (e.g. faults).
#[derive(Debug, Clone, PartialEq)]
pub struct SimConfig {
    /// Simulated seconds per wall-clock second (1.0 = realtime).
    pub speed: f32,
    /// After commit, start on its own (else wait for the `0x46` nudge).
    pub auto_proceed: bool,
    /// Refuse the next brew with this fault.
    pub fault: Option<SimFault>,
    /// Silent grind duration, simulated seconds.
    pub grind_s: f32,
    /// After the ready beep, the cup is "lifted" (→ idle) this many simulated seconds later.
    pub cup_lift_s: f32,
    /// Stream idle scale / status frames like the real machine does.
    pub idle_stream: bool,
    /// Wall-clock tick of the simulation loop.
    pub tick: Duration,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            speed: 1.0,
            auto_proceed: true,
            fault: None,
            grind_s: 18.0,
            cup_lift_s: 25.0,
            idle_stream: true,
            tick: Duration::from_millis(50),
        }
    }
}

struct Link {
    tx: Option<Sender<Vec<u8>>>,
    rx: Receiver<Vec<u8>>,
    handle: Option<JoinHandle<()>>,
}

/// The simulated machine, as a [`Transport`].
pub struct SimTransport {
    cfg: Arc<Mutex<SimConfig>>,
    link: Option<Link>,
}

impl SimTransport {
    /// A simulator with `cfg`.
    pub fn new(cfg: SimConfig) -> Self {
        Self { cfg: Arc::new(Mutex::new(cfg)), link: None }
    }

    /// A handle to adjust the running simulation (e.g. inject a fault).
    pub fn control(&self) -> Arc<Mutex<SimConfig>> {
        Arc::clone(&self.cfg)
    }
}

impl Transport for SimTransport {
    fn scan(&mut self, timeout: Duration) -> Result<Vec<Discovered>, TransportError> {
        thread::sleep(timeout.min(Duration::from_millis(400)));
        Ok(vec![Discovered { address: SIM_ADDRESS.to_string(), name: Some(SIM_NAME.to_string()), rssi: Some(-48) }])
    }

    fn connect(&mut self, address: &str) -> Result<(), TransportError> {
        if !address.eq_ignore_ascii_case(SIM_ADDRESS) {
            return Err(TransportError::NotFound(address.to_string()));
        }
        self.disconnect();
        let (tx, machine_rx) = mpsc::channel::<Vec<u8>>();
        let (machine_tx, rx) = mpsc::channel::<Vec<u8>>();
        let cfg = Arc::clone(&self.cfg);
        let handle = thread::Builder::new()
            .name("xbloom-sim".into())
            .spawn(move || SimMachine::new(cfg, machine_tx).run(&machine_rx))
            .map_err(|e| TransportError::Ble(e.to_string()))?;
        self.link = Some(Link { tx: Some(tx), rx, handle: Some(handle) });
        Ok(())
    }

    fn disconnect(&mut self) {
        if let Some(mut link) = self.link.take() {
            drop(link.tx.take());
            if let Some(h) = link.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn is_connected(&self) -> bool {
        self.link.is_some()
    }

    fn write(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        let link = self.link.as_ref().ok_or(TransportError::NotConnected)?;
        link.tx
            .as_ref()
            .ok_or(TransportError::NotConnected)?
            .send(frame.to_vec())
            .map_err(|_| TransportError::Disconnected)
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, TransportError> {
        let link = self.link.as_ref().ok_or(TransportError::NotConnected)?;
        let r = if timeout.is_zero() {
            link.rx.try_recv().map_err(|e| match e {
                mpsc::TryRecvError::Empty => RecvTimeoutError::Timeout,
                mpsc::TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
            })
        } else {
            link.rx.recv_timeout(timeout)
        };
        match r {
            Ok(f) => Ok(Some(f)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(TransportError::Disconnected),
        }
    }
}

impl Drop for SimTransport {
    fn drop(&mut self) {
        self.disconnect();
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Confirm,
    Grind,
    Pour,
    Pause,
    Drawdown,
    Ready,
}

#[derive(Debug, Clone)]
struct Brew {
    phase: Phase,
    since: f32,
    pour: usize,
    poured: f32,
    water: f32,
    coffee: f32,
}

struct SimMachine {
    cfg: Arc<Mutex<SimConfig>>,
    out: Sender<Vec<u8>>,
    now: f32,
    state: u8,
    pro: bool,
    dose: i32,
    pours: Vec<WirePour>,
    no_grind: bool,
    slots: u8,
    brew: Option<Brew>,
    scheduled: Vec<(f32, Action)>,
    last_scale: f32,
    last_idle: f32,
}

#[derive(Debug, Clone)]
enum Action {
    Status(u8),
    Notify(Vec<u8>),
    BeginStart,
}

impl SimMachine {
    fn new(cfg: Arc<Mutex<SimConfig>>, out: Sender<Vec<u8>>) -> Self {
        Self {
            cfg,
            out,
            now: 0.0,
            state: 0x01,
            pro: true,
            dose: 0,
            pours: Vec::new(),
            no_grind: false,
            slots: 0,
            brew: None,
            scheduled: Vec::new(),
            last_scale: 0.0,
            last_idle: 0.0,
        }
    }

    fn cfg(&self) -> SimConfig {
        self.cfg.lock().map(|c| c.clone()).unwrap_or_default()
    }

    fn run(mut self, rx: &Receiver<Vec<u8>>) {
        let mut last = Instant::now();
        loop {
            let cfg = self.cfg();
            match rx.recv_timeout(cfg.tick) {
                Ok(frame) => self.handle(&frame, &cfg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let wall = last.elapsed().as_secs_f32();
            last = Instant::now();
            let dt = wall * cfg.speed.max(0.01);
            self.now += dt;
            self.run_scheduled(&cfg);
            self.advance(dt, &cfg);
            self.stream(&cfg);
        }
    }

    fn send(&self, frame: Vec<u8>) {
        let _ = self.out.send(frame);
    }

    fn status(&mut self, s: u8) {
        self.state = s;
        self.send(build_notification(STATUS_TYPE, 0x00, &[s, 0x00, 0x00]));
    }

    fn ack(&self, cmd: u8, sub: u8) {
        self.send(build_notification(cmd, sub, &[0x01]));
    }

    fn schedule(&mut self, after: f32, action: Action) {
        self.scheduled.push((self.now + after, action));
    }

    fn run_scheduled(&mut self, cfg: &SimConfig) {
        let now = self.now;
        let (due, rest): (Vec<_>, Vec<_>) = self.scheduled.drain(..).partition(|(t, _)| *t <= now);
        self.scheduled = rest;
        for (_, action) in due {
            match action {
                Action::Status(s) => self.status(s),
                Action::Notify(f) => self.send(f),
                Action::BeginStart => self.begin_start(cfg),
            }
        }
    }

    fn handle(&mut self, frame: &[u8], cfg: &SimConfig) {
        let Some(cmd) = protocol::parse_command(frame) else {
            log::warn!("sim: dropped a malformed frame ({} bytes)", frame.len());
            return;
        };
        let payload = cmd.payload.to_vec();
        match cmd.cmd {
            0xA4 => {
                self.ack(0xA4, 0);
                if self.brew.is_none() {
                    self.status(if self.pro { 0x01 } else { 0x41 });
                }
            }
            0x56 => self.ack(0x56, 0),
            0xA6 => {
                self.ack(0xA6, 0);
                self.dose = i32::from(payload.get(9).copied().unwrap_or(0));
                self.status(0x1D);
            }
            0xA8 => self.ack(0xA8, 0),
            0x41 | 0x44 => {
                let op = cmd.cmd as u8;
                self.ack(op, 0);
                match protocol::decode_pours(&payload) {
                    Some((pours, _grind, tail)) if self.dose > 0 => {
                        let total: i32 = pours.iter().map(|p| p.ml).sum();
                        let expect =
                            (round_half_even(f64::from(total) / f64::from(self.dose) * 10.0) as i64 & 0xFF) as u8;
                        if expect == tail {
                            self.pours = pours;
                            self.no_grind = op == 0x44;
                            self.brew = None;
                            self.status(0x1F);
                        } else {
                            log::warn!(
                                "sim: ratio byte {tail:#04x} doesn't match the pours ({expect:#04x}) — not arming"
                            );
                        }
                    }
                    _ => log::warn!("sim: pours frame without a dose / undecodable — not arming"),
                }
            }
            0x42 if self.state == 0x1F => {
                self.ack(0x42, 0);
                self.status(0x1E);
                self.brew = Some(Brew {
                    phase: Phase::Confirm,
                    since: self.now,
                    pour: 0,
                    poured: 0.0,
                    water: 0.0,
                    coffee: 0.0,
                });
                match cfg.fault {
                    Some(SimFault::NoWater) => self.schedule(0.6, Action::Status(0x0C)),
                    Some(SimFault::NoBeans) => self.schedule(0.8, Action::Status(0x0F)),
                    None if cfg.auto_proceed => self.schedule(1.0, Action::BeginStart),
                    None => {}
                }
            }
            0x46 => {
                self.ack(0x46, 0);
                match self.brew.as_ref().map(|b| b.phase) {
                    Some(Phase::Confirm) if cfg.fault.is_none() => self.begin_start(cfg),
                    Some(Phase::Grind | Phase::Pour | Phase::Pause) => {
                        // Firmware quirk: a start sent into a running brew aborts it to armed.
                        self.brew = None;
                        self.status(0x1F);
                    }
                    _ => {}
                }
            }
            0x47 => {
                self.ack(0x47, 0);
                self.brew = None;
                self.scheduled.retain(|(_, a)| !matches!(a, Action::BeginStart));
                self.status(0x01);
            }
            0x2CF6 => {
                if !self.pro {
                    log::warn!("sim: slot write in AUTO mode — rejected (RETRY)");
                    return;
                }
                self.ack(0xF6, 0x2C);
                self.slots += 1;
                if self.slots == 3 {
                    self.slots = 0;
                    self.schedule(0.1, Action::Notify(build_notification(0xF8, 0x00, &[0x01])));
                    self.schedule(0.2, Action::Status(0x43));
                    self.schedule(1.0, Action::Status(0x25));
                    self.schedule(1.5, Action::Status(0x01));
                }
            }
            0x2CF7 => {
                self.pro = payload.get(1..5) == Some(&[0, 0, 0, 0][..]);
                self.slots = 0;
                self.status(if self.pro { 0x01 } else { 0x41 });
            }
            other => {
                log::debug!("sim: ignoring command {other:#06x}");
                if other <= 0xFF {
                    self.ack(other as u8, 0);
                }
            }
        }
    }

    fn begin_start(&mut self, _cfg: &SimConfig) {
        if let Some(b) = self.brew.as_mut() {
            if b.phase == Phase::Confirm {
                b.phase = Phase::Grind;
                b.since = self.now;
                self.status(0x22);
            }
        }
    }

    fn advance(&mut self, dt: f32, cfg: &SimConfig) {
        let Some(mut b) = self.brew.take() else { return };
        let retain = 2.0 * self.dose as f32;
        let drip_target = (b.water - retain).max(0.0);
        b.coffee += (drip_target - b.coffee) * (1.0 - libm::expf(-dt / 6.0));
        let mut new_status = None;
        match b.phase {
            Phase::Confirm => {}
            Phase::Grind => {
                let grind = if self.no_grind { 2.0 } else { cfg.grind_s };
                if self.now - b.since >= grind {
                    b.phase = Phase::Pour;
                    b.since = self.now;
                    new_status = Some(0x10);
                }
            }
            Phase::Pour => {
                if let Some(p) = self.pours.get(b.pour) {
                    let step = (p.flow as f32 * dt).min(p.ml as f32 - b.poured);
                    b.poured += step;
                    b.water += step;
                    if b.poured >= p.ml as f32 - 1e-3 {
                        b.phase = Phase::Pause;
                        b.since = self.now;
                    }
                }
            }
            Phase::Pause => {
                let pause = self.pours.get(b.pour).map_or(0.0, |p| p.pause as f32);
                if self.now - b.since >= pause {
                    b.pour += 1;
                    b.poured = 0.0;
                    b.since = self.now;
                    if b.pour < self.pours.len() {
                        b.phase = Phase::Pour;
                        new_status = Some(0x23);
                    } else {
                        b.phase = Phase::Drawdown;
                    }
                }
            }
            Phase::Drawdown => {
                if drip_target - b.coffee < 0.25 || self.now - b.since > 45.0 {
                    b.phase = Phase::Ready;
                    b.since = self.now;
                    new_status = Some(0x24);
                }
            }
            Phase::Ready => {
                if self.now - b.since >= cfg.cup_lift_s {
                    self.status(0x01);
                    return;
                }
            }
        }
        self.brew = Some(b);
        if let Some(s) = new_status {
            self.status(s);
        }
    }

    fn stream(&mut self, cfg: &SimConfig) {
        let scale_period = 0.1 * cfg.speed.max(0.01);
        match &self.brew {
            Some(b) if b.phase != Phase::Confirm => {
                if self.now - self.last_scale >= scale_period {
                    self.last_scale = self.now;
                    let (w, c) = (b.water, b.coffee);
                    self.send(build_notification(WATER_TYPE, 0, &(w * 1000.0).to_le_bytes()));
                    self.send(build_notification(COFFEE_TYPE, 0, &c.to_le_bytes()));
                }
            }
            _ if cfg.idle_stream && self.now - self.last_idle >= 2.0 * cfg.speed.max(0.01) => {
                self.last_idle = self.now;
                self.send(build_notification(WATER_TYPE, 0, &0f32.to_le_bytes()));
                self.send(build_notification(COFFEE_TYPE, 0, &0f32.to_le_bytes()));
                let s = self.state;
                self.status(s);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brew::{BrewMonitor, BrewOutcome, BrewSignal};
    use crate::driver::{Machine, Timings};
    use crate::recipe::{Pattern, Pour, Recipe};
    use crate::telemetry::MachineState;

    fn recipe() -> Recipe {
        let p = |ml, pause| Pour {
            label: None,
            ml,
            temp_c: 92,
            pattern: Pattern::Spiral,
            pause_s: pause,
            rpm: 100,
            flow_ml_s: 3.5,
            agitation: false,
        };
        Recipe {
            name: "Sim".into(),
            dose_g: 15,
            grind: 55,
            ratio: Some(16.0),
            stage_temps: [110.0, 90.0],
            kind: None,
            dripper: None,
            water_ml: None,
            hot_water_ml: None,
            ice_g: None,
            time: None,
            note: None,
            pours: vec![p(40, 10), p(200, 2)],
        }
    }

    fn fast(cfg: SimConfig) -> Machine<SimTransport> {
        let mut m = Machine::new(
            SimTransport::new(SimConfig { speed: 20.0, grind_s: 6.0, cup_lift_s: 4.0, ..cfg }),
            Timings::scaled(0.02),
        );
        m.connect(SIM_ADDRESS).unwrap();
        m
    }

    #[test]
    fn load_arms_without_brewing() {
        let mut m = fast(SimConfig::default());
        let armed = m.load_recipe(&recipe()).unwrap();
        assert_eq!(armed.state(), Some(MachineState::Armed));
        // Nothing brews on its own after a load: no starting/brewing state shows up.
        let until = Instant::now() + Duration::from_millis(400);
        while Instant::now() < until {
            if let Some(ev) = m.next_event(Duration::from_millis(50)).unwrap() {
                assert!(!matches!(ev.state(), Some(MachineState::Starting | MachineState::Brewing(_))));
                assert!(ev.water_g.unwrap_or(0.0) < 0.5);
            }
        }
    }

    #[test]
    fn full_remote_brew_completes() {
        let mut m = fast(SimConfig::default());
        let r = recipe();
        m.load_recipe(&r).unwrap();
        let started = m.start().unwrap();
        let t0 = Instant::now();
        let mut mon = BrewMonitor::new(&r, 0.0, true, started.observed.as_ref());
        let outcome = loop {
            let now = t0.elapsed().as_secs_f32() * 20.0;
            if let Some(ev) = m.next_event(Duration::from_millis(50)).unwrap() {
                if let Some(BrewSignal::Finished(o)) = mon.on_event(&ev, now) {
                    break o;
                }
            } else if let Some(BrewSignal::Finished(o)) = mon.tick(now) {
                break o;
            }
            assert!(t0.elapsed() < Duration::from_secs(20), "brew never finished");
        };
        assert_eq!(outcome, BrewOutcome::Completed);
        assert!((mon.final_water_g() - 240.0).abs() < 2.0, "water {}", mon.final_water_g());
        assert!(!mon.samples().is_empty());
    }

    #[test]
    fn stalled_confirm_gets_the_start_nudge() {
        let mut m = fast(SimConfig { auto_proceed: false, ..SimConfig::default() });
        m.load_recipe(&recipe()).unwrap();
        let out = m.start().unwrap();
        assert_eq!(out.observed.and_then(|e| e.state()), Some(MachineState::Starting));
    }

    #[test]
    fn no_beans_is_reported_by_start() {
        let mut m = fast(SimConfig { fault: Some(SimFault::NoBeans), ..SimConfig::default() });
        m.load_recipe(&recipe()).unwrap();
        let out = m.start().unwrap();
        assert_eq!(out.observed.and_then(|e| e.state()), Some(MachineState::NoBeans));
        m.cancel().unwrap();
    }

    #[test]
    fn slots_save_atomically_and_return_to_auto() {
        let mut m = fast(SimConfig::default());
        let r = recipe();
        m.save_slots([&r, &r, &r], [true, true, false]).unwrap();
    }

    #[test]
    fn mismatched_ratio_byte_does_not_arm() {
        let mut m = fast(SimConfig::default());
        let mut wire = recipe().to_wire();
        wire.tail = Some(0x20);
        let frames = protocol::build_load_frames(&wire).unwrap();
        for f in &frames {
            m.transport_mut().write(f).unwrap();
        }
        let until = Instant::now() + Duration::from_millis(500);
        while Instant::now() < until {
            if let Some(ev) = m.next_event(Duration::from_millis(50)).unwrap() {
                assert_ne!(ev.state(), Some(MachineState::Armed));
            }
        }
    }
}
