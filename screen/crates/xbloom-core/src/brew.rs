//! The brew monitor: turns the live notification stream into phases, progress and an outcome.
//!
//! A port of the brew loop in `xbloom_ble/tui/app.py`, made a pure state machine (no clock,
//! no I/O) so both screens share it and it is unit-testable. Feed it every decoded
//! [`StatusEvent`] with a timestamp and call [`BrewMonitor::tick`] periodically; it reports
//! phase changes and, once, the [`BrewOutcome`].
//!
//! What the hardware taught the reference (and therefore this port):
//!
//! * **The brew has no reliable "done" status.** `0x24` (ready, the beep) is the best signal;
//!   `0x41` often only arrives on cup-lift; idle (`0x01`) only after the cup is lifted. So the
//!   monitor also completes on a **drawdown plateau**: once ≥ 85 % of the target water has
//!   poured and the cup weight hasn't risen for [`DRAWDOWN_PLATEAU_S`], the drip is finished.
//! * **The grind is silent.** After commit the machine goes quiet for ~20 s. A remote start is
//!   only declared "didn't start" after [`GRIND_GUARD_S`] with no real progress.
//! * **No beans / no water** are refusals the machine reports right after commit; the brew is
//!   aborted and the caller should send cancel.
//!
//! One deliberate refinement over the reference: only a scale reading above
//! [`PROGRESS_WATER_G`] counts as progress. The scale also streams near-zero readings at idle,
//! which in the reference silently disarmed the "didn't start" guard.

use alloc::string::String;
use alloc::vec::Vec;

use crate::recipe::Recipe;
use crate::telemetry::{EventKind, MachineState, StatusEvent};

/// A remote start with no real progress for this long is declared "didn't start".
pub const GRIND_GUARD_S: f32 = 30.0;
/// Cup weight flat for this long (after the pours) means the drawdown has finished.
pub const DRAWDOWN_PLATEAU_S: f32 = 6.0;
/// A water reading above this many grams means the machine is genuinely pouring.
pub const PROGRESS_WATER_G: f32 = 1.0;
/// Load-only: stop waiting for the human to approve on the machine after this long.
pub const APPROVE_TIMEOUT_S: f32 = 300.0;
/// Hard ceiling on a single brew session.
pub const MAX_BREW_S: f32 = 900.0;
/// Minimum spacing between recorded samples (the scale streams ~20 frames/s combined).
pub const SAMPLE_PERIOD_S: f32 = 0.2;

/// Where a brew is, for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrewPhase {
    /// Loaded and armed; waiting for the human to approve on the machine (load-only).
    Armed,
    /// Committed; the machine wants a confirm / is counting down.
    AwaitingConfirm,
    /// Grinding / spinning up (often silent for ~20 s).
    Starting,
    /// Pouring.
    Brewing,
    /// The pours are done and the coffee is draining.
    Drawdown,
    /// Finished (see the outcome).
    Finished,
}

impl BrewPhase {
    /// A short human message for the phase.
    pub fn message(self) -> &'static str {
        match self {
            Self::Armed => "Armed — approve on the machine",
            Self::AwaitingConfirm => "Confirm on the machine",
            Self::Starting => "Grinding…",
            Self::Brewing => "Brewing…",
            Self::Drawdown => "Drawing down…",
            Self::Finished => "Done",
        }
    }
}

/// How a brew ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrewOutcome {
    /// The brew completed (record it in history).
    Completed,
    /// The machine refused: no beans. The caller should send cancel.
    NoBeans,
    /// The machine refused: no water. The caller should send cancel.
    NoWater,
    /// A remote start never produced real progress. The caller should send cancel.
    DidNotStart,
    /// Load-only: nobody approved on the machine in time (the machine stays armed).
    NotApproved,
    /// The machine went idle without the brew running (e.g. cancelled on the device).
    Stopped,
    /// The user cancelled from the screen.
    Cancelled,
    /// The session exceeded [`MAX_BREW_S`].
    TimedOut,
}

impl BrewOutcome {
    /// True when the caller should send the cancel frame to return the machine to idle.
    pub fn needs_cancel(self) -> bool {
        matches!(self, Self::NoBeans | Self::NoWater | Self::DidNotStart | Self::TimedOut)
    }

    /// A short human message.
    pub fn message(self) -> &'static str {
        match self {
            Self::Completed => "Coffee ready — enjoy!",
            Self::NoBeans => "No beans — add beans, then brew again",
            Self::NoWater => "No water — fill the tank, then brew again",
            Self::DidNotStart => "The brew didn't start — beans, water and cup in place?",
            Self::NotApproved => "Not approved on the machine in time",
            Self::Stopped => "Stopped on the machine",
            Self::Cancelled => "Brew cancelled",
            Self::TimedOut => "Brew timed out",
        }
    }

    /// A stable id (for history, logs and the API).
    pub fn id(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::NoBeans => "no_beans",
            Self::NoWater => "no_water",
            Self::DidNotStart => "did_not_start",
            Self::NotApproved => "not_approved",
            Self::Stopped => "stopped",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }
}

/// One recorded point of the brew curve.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Sample {
    /// Seconds since the brew began.
    pub t: f32,
    /// Water dispensed, grams.
    pub water: f32,
    /// Coffee in the cup, grams.
    pub coffee: f32,
}

/// What the monitor tells its caller after an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrewSignal {
    /// The phase changed.
    Phase(BrewPhase),
    /// The brew is over. Reported exactly once.
    Finished(BrewOutcome),
}

/// A stage of the recipe with its cumulative water mark (for "Bloom → Pour 1 → …").
#[derive(Debug, Clone, PartialEq)]
pub struct Stage {
    /// The pour's label (or "Bloom" / "Pour N").
    pub label: String,
    /// Water poured by the end of this stage, grams (≈ ml).
    pub until_g: f32,
}

/// The monitor.
#[derive(Debug, Clone)]
pub struct BrewMonitor {
    dose_g: f32,
    target_g: f32,
    stages: Vec<Stage>,
    remote_start: bool,
    t0: f32,
    phase: BrewPhase,
    outcome: Option<BrewOutcome>,
    brew_began: bool,
    saw_progress: bool,
    water: f32,
    coffee: f32,
    water_peak: f32,
    coffee_peak: f32,
    last_rise: f32,
    last_sample_t: f32,
    samples: Vec<Sample>,
    end_t: Option<f32>,
}

/// Default stage labels when a pour has none: "Bloom", then "Pour 1", "Pour 2", …
pub fn stage_label(recipe: &Recipe, index: usize) -> String {
    match recipe.pours.get(index).and_then(|p| p.label.clone()) {
        Some(l) => l,
        None if index == 0 => String::from("Bloom"),
        None => alloc::format!("Pour {index}"),
    }
}

impl BrewMonitor {
    /// Start monitoring a brew of `recipe` at time `now` (seconds, any monotonic origin).
    ///
    /// `remote_start` is whether the screen sent commit/start; `start_observed` is the real
    /// machine event the driver's start returned, if any (a synthetic fallback must be `None`,
    /// so it cannot disarm the "didn't start" guard).
    pub fn new(recipe: &Recipe, now: f32, remote_start: bool, start_observed: Option<&StatusEvent>) -> Self {
        let mut acc = 0.0;
        let stages = recipe
            .pours
            .iter()
            .enumerate()
            .map(|(i, p)| {
                acc += p.ml as f32;
                Stage { label: stage_label(recipe, i), until_g: acc }
            })
            .collect();
        let saw_progress = matches!(
            start_observed.and_then(StatusEvent::state),
            Some(MachineState::Starting | MachineState::Brewing(_))
        );
        let phase = if remote_start {
            match start_observed.and_then(StatusEvent::state) {
                Some(MachineState::Brewing(_)) => BrewPhase::Brewing,
                Some(MachineState::AwaitingConfirm) => BrewPhase::AwaitingConfirm,
                _ => BrewPhase::Starting,
            }
        } else {
            BrewPhase::Armed
        };
        Self {
            dose_g: recipe.dose_g as f32,
            target_g: recipe.dose_g as f32 * recipe.effective_ratio() as f32,
            stages,
            remote_start,
            t0: now,
            phase,
            outcome: None,
            brew_began: remote_start,
            saw_progress,
            water: 0.0,
            coffee: 0.0,
            water_peak: 0.0,
            coffee_peak: 0.0,
            last_rise: now,
            last_sample_t: f32::NEG_INFINITY,
            samples: Vec::new(),
            end_t: None,
        }
    }

    /// The current phase.
    pub fn phase(&self) -> BrewPhase {
        self.phase
    }

    /// The outcome, once finished.
    pub fn outcome(&self) -> Option<BrewOutcome> {
        self.outcome
    }

    /// Target water for the brew (dose × ratio), grams.
    pub fn target_g(&self) -> f32 {
        self.target_g
    }

    /// Latest water reading, grams.
    pub fn water_g(&self) -> f32 {
        self.water
    }

    /// Latest coffee (cup) reading, grams.
    pub fn coffee_g(&self) -> f32 {
        self.coffee
    }

    /// Peak water reading (the machine zeroes its water scale at the very end).
    pub fn final_water_g(&self) -> f32 {
        self.water_peak
    }

    /// Peak coffee reading.
    pub fn final_coffee_g(&self) -> f32 {
        self.coffee_peak.max(self.coffee)
    }

    /// The recorded curve.
    pub fn samples(&self) -> &[Sample] {
        &self.samples
    }

    /// The recipe's stages with cumulative water marks.
    pub fn stages(&self) -> &[Stage] {
        &self.stages
    }

    /// Seconds since the brew began (frozen once finished).
    pub fn elapsed(&self, now: f32) -> f32 {
        self.end_t.unwrap_or(now) - self.t0
    }

    /// Progress toward the target water, 0–1.
    pub fn progress(&self) -> f32 {
        if self.target_g <= 0.0 {
            0.0
        } else {
            (self.water_peak / self.target_g).clamp(0.0, 1.0)
        }
    }

    /// Index of the stage currently pouring (by water poured so far).
    pub fn stage_index(&self) -> usize {
        let w = self.water_peak + 0.5;
        self.stages.iter().position(|s| w < s.until_g).unwrap_or(self.stages.len().saturating_sub(1))
    }

    fn finish(&mut self, outcome: BrewOutcome, now: f32) -> Option<BrewSignal> {
        if self.outcome.is_some() {
            return None;
        }
        self.outcome = Some(outcome);
        self.phase = BrewPhase::Finished;
        self.end_t = Some(now);
        Some(BrewSignal::Finished(outcome))
    }

    fn set_phase(&mut self, phase: BrewPhase) -> Option<BrewSignal> {
        if self.phase == phase || self.outcome.is_some() {
            return None;
        }
        self.phase = phase;
        Some(BrewSignal::Phase(phase))
    }

    /// The user cancelled from the screen (the caller sends the cancel frame).
    pub fn cancel(&mut self, now: f32) -> Option<BrewSignal> {
        self.finish(BrewOutcome::Cancelled, now)
    }

    /// Feed one decoded notification.
    pub fn on_event(&mut self, ev: &StatusEvent, now: f32) -> Option<BrewSignal> {
        if self.outcome.is_some() {
            return None;
        }
        if ev.water_g.is_some() || ev.coffee_g.is_some() {
            if let Some(s) = self.on_scale(ev, now) {
                return Some(s);
            }
        }
        match ev.kind {
            EventKind::Scale => self.tick(now),
            EventKind::Ack(0x41) if self.saw_progress => self.finish(BrewOutcome::Completed, now),
            EventKind::Ack(_) => self.tick(now),
            EventKind::Status(state) => self.on_state(state, now),
        }
    }

    fn on_scale(&mut self, ev: &StatusEvent, now: f32) -> Option<BrewSignal> {
        if let Some(w) = ev.water_g {
            self.water = w;
            self.water_peak = self.water_peak.max(w);
            if w > PROGRESS_WATER_G {
                self.saw_progress = true;
                self.brew_began = true;
            }
        }
        if let Some(c) = ev.coffee_g {
            self.coffee = c;
        }
        let t = now - self.t0;
        if t - self.last_sample_t >= SAMPLE_PERIOD_S - 1e-3 {
            self.samples.push(Sample { t, water: self.water, coffee: self.coffee });
            self.last_sample_t = t;
        }
        if self.coffee > self.coffee_peak + 0.3 {
            self.coffee_peak = self.coffee;
            self.last_rise = now;
        }
        if self.saw_progress {
            let poured = self.target_g > 0.0 && self.water_peak >= 0.85 * self.target_g;
            if poured {
                if let Some(s) = self.set_phase(BrewPhase::Drawdown) {
                    return Some(s);
                }
                if self.coffee >= 0.5 * self.dose_g && now - self.last_rise >= DRAWDOWN_PLATEAU_S {
                    return self.finish(BrewOutcome::Completed, now);
                }
            } else if self.water > PROGRESS_WATER_G && self.phase != BrewPhase::Brewing {
                return self.set_phase(BrewPhase::Brewing);
            }
        }
        None
    }

    fn on_state(&mut self, state: MachineState, now: f32) -> Option<BrewSignal> {
        match state {
            MachineState::AwaitingConfirm => {
                self.brew_began = true;
                self.set_phase(BrewPhase::AwaitingConfirm)
            }
            MachineState::Starting => {
                self.brew_began = true;
                self.saw_progress = true;
                self.set_phase(BrewPhase::Starting)
            }
            MachineState::Brewing(_) => {
                self.brew_began = true;
                self.saw_progress = true;
                if self.phase == BrewPhase::Drawdown {
                    None
                } else {
                    self.set_phase(BrewPhase::Brewing)
                }
            }
            MachineState::NoBeans => self.finish(BrewOutcome::NoBeans, now),
            MachineState::NoWater => self.finish(BrewOutcome::NoWater, now),
            MachineState::Ready if self.saw_progress => self.finish(BrewOutcome::Completed, now),
            MachineState::Complete => self.finish(BrewOutcome::Completed, now),
            MachineState::Idle if self.brew_began => {
                let o = if self.saw_progress { BrewOutcome::Completed } else { BrewOutcome::Stopped };
                self.finish(o, now)
            }
            _ => self.tick(now),
        }
    }

    /// Time-based checks (call every few hundred ms even when no frames arrive).
    pub fn tick(&mut self, now: f32) -> Option<BrewSignal> {
        if self.outcome.is_some() {
            return None;
        }
        let t = now - self.t0;
        if self.remote_start && !self.saw_progress && t > GRIND_GUARD_S {
            return self.finish(BrewOutcome::DidNotStart, now);
        }
        if !self.remote_start && !self.brew_began && t > APPROVE_TIMEOUT_S {
            return self.finish(BrewOutcome::NotApproved, now);
        }
        if t > MAX_BREW_S {
            return self.finish(BrewOutcome::TimedOut, now);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::{Pattern, Pour};
    use alloc::vec;

    fn recipe() -> Recipe {
        let p = |ml| Pour {
            label: None,
            ml,
            temp_c: 92,
            pattern: Pattern::Spiral,
            pause_s: 5,
            rpm: 100,
            flow_ml_s: 3.0,
            agitation: false,
        };
        Recipe {
            name: "T".into(),
            dose_g: 16,
            grind: 60,
            ratio: None,
            stage_temps: [110.0, 90.0],
            kind: None,
            dripper: None,
            water_ml: None,
            hot_water_ml: None,
            ice_g: None,
            time: None,
            note: None,
            pours: vec![p(40), p(200)],
        }
    }

    fn water(g: f32) -> StatusEvent {
        StatusEvent { kind: EventKind::Scale, water_g: Some(g), coffee_g: None }
    }
    fn coffee(g: f32) -> StatusEvent {
        StatusEvent { kind: EventKind::Scale, water_g: None, coffee_g: Some(g) }
    }

    #[test]
    fn completes_on_ready_after_progress() {
        let mut m = BrewMonitor::new(&recipe(), 0.0, true, None);
        assert_eq!(m.phase(), BrewPhase::Starting);
        assert_eq!(m.on_event(&StatusEvent::status(MachineState::Starting), 1.0), None);
        m.on_event(&water(50.0), 25.0);
        assert_eq!(m.stage_index(), 1);
        assert_eq!(
            m.on_event(&StatusEvent::status(MachineState::Ready), 120.0),
            Some(BrewSignal::Finished(BrewOutcome::Completed))
        );
        assert_eq!(m.on_event(&StatusEvent::status(MachineState::Idle), 121.0), None);
    }

    #[test]
    fn completes_on_drawdown_plateau() {
        let mut m = BrewMonitor::new(&recipe(), 0.0, true, None);
        m.on_event(&water(240.0), 80.0);
        assert_eq!(m.phase(), BrewPhase::Drawdown);
        m.on_event(&coffee(200.0), 81.0);
        assert_eq!(m.on_event(&coffee(200.1), 86.0), None);
        assert_eq!(m.on_event(&coffee(200.2), 87.5), Some(BrewSignal::Finished(BrewOutcome::Completed)));
        assert_eq!(m.final_water_g(), 240.0);
    }

    #[test]
    fn idle_scale_noise_does_not_disarm_the_guard() {
        let mut m = BrewMonitor::new(&recipe(), 0.0, true, None);
        for i in 0..300 {
            assert_eq!(m.on_event(&water(0.0), i as f32 * 0.1), None);
        }
        assert_eq!(m.tick(31.0), Some(BrewSignal::Finished(BrewOutcome::DidNotStart)));
        assert!(BrewOutcome::DidNotStart.needs_cancel());
    }

    #[test]
    fn a_real_start_event_disarms_the_guard() {
        let started = StatusEvent::status(MachineState::Starting);
        let mut m = BrewMonitor::new(&recipe(), 0.0, true, Some(&started));
        assert_eq!(m.tick(45.0), None);
    }

    #[test]
    fn refusals_abort_with_cancel() {
        let mut m = BrewMonitor::new(&recipe(), 0.0, true, None);
        let s = m.on_event(&StatusEvent::status(MachineState::NoBeans), 2.0);
        assert_eq!(s, Some(BrewSignal::Finished(BrewOutcome::NoBeans)));
        assert!(m.outcome().unwrap().needs_cancel());
    }

    #[test]
    fn load_only_waits_for_approval_then_times_out() {
        let mut m = BrewMonitor::new(&recipe(), 0.0, false, None);
        assert_eq!(m.phase(), BrewPhase::Armed);
        assert_eq!(m.tick(100.0), None);
        assert_eq!(m.tick(301.0), Some(BrewSignal::Finished(BrewOutcome::NotApproved)));
        assert!(!BrewOutcome::NotApproved.needs_cancel());
    }

    #[test]
    fn early_idle_without_progress_is_stopped_not_completed() {
        let mut m = BrewMonitor::new(&recipe(), 0.0, false, None);
        m.on_event(&StatusEvent::status(MachineState::AwaitingConfirm), 3.0);
        assert_eq!(
            m.on_event(&StatusEvent::status(MachineState::Idle), 5.0),
            Some(BrewSignal::Finished(BrewOutcome::Stopped))
        );
    }

    #[test]
    fn samples_are_throttled() {
        let mut m = BrewMonitor::new(&recipe(), 0.0, true, None);
        for i in 0..100 {
            m.on_event(&water(i as f32), i as f32 * 0.05);
        }
        assert!(m.samples().len() <= 26 && m.samples().len() >= 24);
    }
}
