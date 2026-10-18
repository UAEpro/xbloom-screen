//! The machine driver: the xBloom conversation (session, LOAD, start, cancel, dial slots) over
//! any BLE [`Transport`].
//!
//! A port of `xbloom_ble/client.py`, written once against a tiny transport trait so the only
//! platform-specific BLE code is "scan / connect / write / receive" — BlueZ on the Raspberry
//! Pi, NimBLE on the ESP32, or the [`crate::sim`] machine in demo mode and tests.
//!
//! The driver is blocking and single-owner: one thread owns the [`Machine`] and runs its
//! operations in sequence (the machine accepts a single BLE link and one conversation at a
//! time anyway).
//!
//! ## 🛑 Safety model
//!
//! [`Machine::load_recipe`] writes only the LOAD frames (plus the `0x56` handshake) and
//! returns once the machine is **armed** — it never starts a brew. [`Machine::start`] is the
//! separate, explicit "go" (commit + start) and physically dispenses near-boiling water; a UI
//! must only call it from a deliberate user gesture. Nothing in this module calls `start`.

use std::fmt;
use std::thread;
use std::time::{Duration, Instant};

use crate::protocol::{self, ProtocolError};
use crate::recipe::{Recipe, RecipeError};
use crate::telemetry::{parse_notification, MachineState, StatusEvent};

/// Vendor GATT service UUID.
pub const SERVICE_UUID: &str = "0000e0ff-3c17-d293-8e48-14fe2e4da212";
/// `ffe1` — command characteristic (Write Command / write-without-response only).
pub const CHAR_COMMAND: &str = "0000ffe1-0000-1000-8000-00805f9b34fb";
/// `ffe2` — status notifications.
pub const CHAR_STATUS: &str = "0000ffe2-0000-1000-8000-00805f9b34fb";
/// Advertised name prefix.
pub const NAME_PREFIX: &str = "XBLOOM";

/// A machine found by a scan.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Discovered {
    /// Platform address (a MAC on Linux / ESP32).
    pub address: String,
    /// Advertised name, if any.
    pub name: Option<String>,
    /// Signal strength, dBm.
    pub rssi: Option<i16>,
}

/// True if an advertisement looks like an xBloom (vendor service or `XBLOOM…` name).
pub fn is_xbloom(name: Option<&str>, service_uuids: &[String]) -> bool {
    name.is_some_and(|n| n.to_ascii_uppercase().starts_with(NAME_PREFIX))
        || service_uuids.iter().any(|u| u.eq_ignore_ascii_case(SERVICE_UUID))
}

/// A BLE link failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// No link is up.
    NotConnected,
    /// The link dropped.
    Disconnected,
    /// The device or its vendor characteristics weren't found.
    NotFound(String),
    /// Anything else from the BLE stack.
    Ble(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConnected => f.write_str("not connected"),
            Self::Disconnected => f.write_str("the machine disconnected"),
            Self::NotFound(s) => write!(f, "not found: {s}"),
            Self::Ble(s) => write!(f, "bluetooth error: {s}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// The platform's BLE link to one machine.
///
/// Implementations must write `ffe1` with **Write Command** (write-without-response) — the
/// firmware rejects Write Requests — and deliver every `ffe2` notification to [`recv`]
/// in order.
///
/// [`recv`]: Transport::recv
pub trait Transport: Send {
    /// Discover nearby machines.
    fn scan(&mut self, timeout: Duration) -> Result<Vec<Discovered>, TransportError>;
    /// Connect to `address` and subscribe to `ffe2` notifications.
    fn connect(&mut self, address: &str) -> Result<(), TransportError>;
    /// Drop the link (idempotent).
    fn disconnect(&mut self);
    /// True while the link is up.
    fn is_connected(&self) -> bool;
    /// Write one frame to `ffe1` (write-without-response).
    fn write(&mut self, frame: &[u8]) -> Result<(), TransportError>;
    /// The next raw `ffe2` notification, waiting up to `timeout`. `Ok(None)` on timeout,
    /// `Err(Disconnected)` if the link dropped.
    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, TransportError>;
}

impl<T: Transport + ?Sized> Transport for Box<T> {
    fn scan(&mut self, timeout: Duration) -> Result<Vec<Discovered>, TransportError> {
        (**self).scan(timeout)
    }
    fn connect(&mut self, address: &str) -> Result<(), TransportError> {
        (**self).connect(address)
    }
    fn disconnect(&mut self) {
        (**self).disconnect()
    }
    fn is_connected(&self) -> bool {
        (**self).is_connected()
    }
    fn write(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        (**self).write(frame)
    }
    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, TransportError> {
        (**self).recv(timeout)
    }
}

/// Pacing of the conversation. The defaults are what the hardware needs (verified in the
/// reference); tests and the simulator use [`Timings::scaled`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timings {
    /// Pause after the connect-time `a4` (machine shows "connected").
    pub open_settle: Duration,
    /// Gap between `a4` and the `0x56` handshake when loading.
    pub a4_gap: Duration,
    /// Settle after the handshake before staging — without it the machine never arms.
    pub load_settle: Duration,
    /// Gap between the dose / temps / pours frames.
    pub frame_gap: Duration,
    /// How long to wait for an expected state (armed, idle, slots-saved).
    pub ack_timeout: Duration,
    /// After commit, how long to watch for the machine to act before nudging with `0x46`.
    pub start_watch: Duration,
    /// After the `0x46` nudge, how long to watch for the brew to begin.
    pub start_nudge_watch: Duration,
    /// Pause after reaching idle before the slot batch.
    pub slot_pre_pause: Duration,
    /// Gap between the three slot frames.
    pub slot_gap: Duration,
    /// Pause after switching back to Auto mode.
    pub mode_pause: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            open_settle: Duration::from_millis(300),
            a4_gap: Duration::from_millis(500),
            load_settle: Duration::from_millis(2000),
            frame_gap: Duration::from_millis(400),
            ack_timeout: Duration::from_secs(10),
            start_watch: Duration::from_secs(8),
            start_nudge_watch: Duration::from_secs(5),
            slot_pre_pause: Duration::from_millis(1000),
            slot_gap: Duration::from_millis(500),
            mode_pause: Duration::from_millis(300),
        }
    }
}

impl Timings {
    /// Every duration multiplied by `factor` (e.g. `0.02` for fast tests). Timeouts are
    /// floored so a slow CI box doesn't flake.
    pub fn scaled(factor: f32) -> Self {
        let d = Self::default();
        let s = |x: Duration| x.mul_f32(factor);
        let t = |x: Duration| x.mul_f32(factor).max(Duration::from_millis(1500));
        Self {
            open_settle: s(d.open_settle),
            a4_gap: s(d.a4_gap),
            load_settle: s(d.load_settle),
            frame_gap: s(d.frame_gap),
            ack_timeout: t(d.ack_timeout),
            start_watch: s(d.start_watch),
            start_nudge_watch: t(d.start_nudge_watch),
            slot_pre_pause: s(d.slot_pre_pause),
            slot_gap: s(d.slot_gap),
            mode_pause: s(d.mode_pause),
        }
    }
}

/// A driver failure.
#[derive(Debug, Clone, PartialEq)]
pub enum DriverError {
    /// The BLE link failed.
    Transport(TransportError),
    /// The recipe is invalid.
    Recipe(RecipeError),
    /// The recipe can't be encoded.
    Protocol(ProtocolError),
    /// An expected machine state never arrived.
    Timeout(&'static str),
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(e) => e.fmt(f),
            Self::Recipe(e) => write!(f, "invalid recipe: {e}"),
            Self::Protocol(e) => e.fmt(f),
            Self::Timeout(what) => write!(f, "timed out waiting for the machine ({what})"),
        }
    }
}

impl std::error::Error for DriverError {}

impl From<TransportError> for DriverError {
    fn from(e: TransportError) -> Self {
        Self::Transport(e)
    }
}

/// What [`Machine::start`] saw the machine do.
#[derive(Debug, Clone, PartialEq)]
pub struct StartOutcome {
    /// The real machine event that showed the brew began (or was refused), if one was seen.
    /// `None` means the start frames were sent but the machine stayed quiet — common, since
    /// it grinds silently; keep streaming telemetry for the live state.
    pub observed: Option<StatusEvent>,
}

/// A conversation with one xBloom Studio over a [`Transport`].
pub struct Machine<T: Transport> {
    transport: T,
    timings: Timings,
    address: Option<String>,
    last_state: Option<MachineState>,
}

impl<T: Transport> Machine<T> {
    /// Wrap a transport.
    pub fn new(transport: T, timings: Timings) -> Self {
        Self { transport, timings, address: None, last_state: None }
    }

    /// The transport (e.g. to scan while disconnected).
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Give the transport back (e.g. to swap between real hardware and the simulator).
    pub fn into_transport(mut self) -> T {
        self.transport.disconnect();
        self.transport
    }

    /// Address of the connected (or last) machine.
    pub fn address(&self) -> Option<&str> {
        self.address.as_deref()
    }

    /// The last machine state seen in a status frame.
    pub fn last_state(&self) -> Option<MachineState> {
        self.last_state
    }

    /// True while the link is up.
    pub fn is_connected(&self) -> bool {
        self.transport.is_connected()
    }

    /// Scan for machines.
    pub fn scan(&mut self, timeout: Duration) -> Result<Vec<Discovered>, DriverError> {
        Ok(self.transport.scan(timeout)?)
    }

    /// Connect (idempotent) and open an app-style session so the machine shows "connected".
    pub fn connect(&mut self, address: &str) -> Result<(), DriverError> {
        if self.transport.is_connected() && self.address.as_deref() == Some(address) {
            return Ok(());
        }
        self.transport.disconnect();
        log::info!("connecting to {address}…");
        self.transport.connect(address)?;
        self.address = Some(address.to_string());
        self.open_session()?;
        log::info!("connected — session open");
        Ok(())
    }

    /// Drop the link.
    pub fn disconnect(&mut self) {
        self.transport.disconnect();
        self.last_state = None;
    }

    /// Write the `a4` session-start frame (exactly what the phone app does on connect).
    ///
    /// A session handshake, not a brew: `a4` never dispenses water.
    pub fn open_session(&mut self) -> Result<(), DriverError> {
        self.write(&protocol::build_session_start())?;
        thread::sleep(self.timings.open_settle);
        Ok(())
    }

    fn write(&mut self, frame: &[u8]) -> Result<(), DriverError> {
        log::debug!("→ {}", hex(frame));
        Ok(self.transport.write(frame)?)
    }

    /// The next decoded notification, waiting up to `timeout`. Tracks the machine state.
    pub fn next_event(&mut self, timeout: Duration) -> Result<Option<StatusEvent>, DriverError> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(raw) = self.transport.recv(remaining)? else { return Ok(None) };
            if let Some(ev) = parse_notification(&raw) {
                if let Some(s) = ev.state() {
                    if self.last_state != Some(s) {
                        log::debug!("← state {}", s.name());
                    }
                    self.last_state = Some(s);
                }
                if !ev.is_heartbeat() {
                    return Ok(Some(ev));
                }
            }
        }
    }

    /// Drop any queued notifications (start each operation with a clean slate).
    pub fn drain(&mut self) -> Result<(), DriverError> {
        while self.next_event(Duration::ZERO)?.is_some() {}
        Ok(())
    }

    fn wait_for_any(&mut self, states: &[MachineState], timeout: Duration) -> Result<Option<StatusEvent>, DriverError> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            if let Some(ev) = self.next_event(remaining)? {
                if ev.state().is_some_and(|s| states.contains(&s)) {
                    return Ok(Some(ev));
                }
            }
        }
    }

    fn wait_for(&mut self, state: MachineState, what: &'static str) -> Result<StatusEvent, DriverError> {
        self.wait_for_any(&[state], self.timings.ack_timeout)?.ok_or(DriverError::Timeout(what))
    }

    /// Load `recipe` and return once the machine is **armed** (`0x1f`). Never starts a brew.
    ///
    /// Writes `a4`, the `0x56` handshake, settles (the machine will not arm a fresh session
    /// otherwise), then dose / temps / pours spaced out, and waits for armed. The human then
    /// approves on the machine — or a UI may call [`Machine::start`] on an explicit gesture.
    pub fn load_recipe(&mut self, recipe: &Recipe) -> Result<StatusEvent, DriverError> {
        recipe.validate().map_err(DriverError::Recipe)?;
        let [a4, a6, a8, pours] = protocol::build_load_frames(&recipe.to_wire()).map_err(DriverError::Protocol)?;
        self.drain()?;
        log::info!("loading '{}' — a4 + 0x56 handshake, then settle", recipe.name);
        self.write(&a4)?;
        thread::sleep(self.timings.a4_gap);
        self.write(&protocol::build_status_query())?;
        thread::sleep(self.timings.load_settle);
        for frame in [a6, a8, pours] {
            self.write(&frame)?;
            thread::sleep(self.timings.frame_gap);
        }
        let armed = self.wait_for(MachineState::Armed, "armed after load")?;
        log::info!("recipe loaded — machine armed");
        Ok(armed)
    }

    /// Start the armed brew: commit (`0x42`), then adapt to the machine.
    ///
    /// Some machines proceed straight from commit to grinding / brewing; others wait in
    /// awaiting-confirm for a start press. So this watches: if the machine acts (starting,
    /// brewing, or refuses with no water / no beans) it does **not** send `0x46` (sending it
    /// into a running brew aborts it); only if it stalls does it send the `0x46` nudge.
    ///
    /// ⚠️ Dispenses near-boiling water. Call only from an explicit user action with the
    /// machine physically ready.
    pub fn start(&mut self) -> Result<StartOutcome, DriverError> {
        const ACTED: [MachineState; 6] = [
            MachineState::Starting,
            MachineState::Brewing(0x3B),
            MachineState::Brewing(0x10),
            MachineState::Brewing(0x23),
            MachineState::NoWater,
            MachineState::NoBeans,
        ];
        self.drain()?;
        log::info!("→ 0x42 commit (start the brew)");
        self.write(&protocol::build_commit())?;
        if let Some(ev) = self.wait_for_any(&ACTED, self.timings.start_watch)? {
            log::info!("machine acted on commit ({}) — not sending 0x46", ev.state_name());
            return Ok(StartOutcome { observed: Some(ev) });
        }
        log::info!("machine waiting in confirm — → 0x46 start");
        self.write(&protocol::build_start())?;
        let observed = self.wait_for_any(&ACTED, self.timings.start_nudge_watch)?;
        Ok(StartOutcome { observed })
    }

    /// Abort a committed / running brew (`0x47`), returning the machine toward idle.
    pub fn cancel(&mut self) -> Result<(), DriverError> {
        log::info!("→ 0x47 cancel");
        self.write(&protocol::build_cancel())
    }

    /// Program the three Easy-Mode dial presets (A, B, C) in one batch. Never brews.
    ///
    /// The machine stores presets only after the whole A/B/C set arrives (then saves
    /// atomically: `0x43` saving → `0x25` saved → idle), and only in PRO mode — so this opens
    /// a session, forces PRO, writes all three, confirms the save, and returns to AUTO so the
    /// presets are ready on the dial.
    pub fn save_slots(&mut self, recipes: [&Recipe; 3], scale: [bool; 3]) -> Result<(), DriverError> {
        let mut frames = Vec::with_capacity(3);
        for (i, r) in recipes.iter().enumerate() {
            r.validate().map_err(DriverError::Recipe)?;
            frames.push(protocol::build_save_slot(&r.to_wire(), i as u8, scale[i]).map_err(DriverError::Protocol)?);
        }
        self.drain()?;
        self.write(&protocol::build_session_start())?;
        log::info!("→ PRO mode (slot writes require it)");
        self.write(&protocol::build_set_mode(true))?;
        if self.wait_for_any(&[MachineState::Idle], self.timings.ack_timeout)?.is_none() {
            log::warn!("machine idle not confirmed; proceeding");
        }
        thread::sleep(self.timings.slot_pre_pause);
        for (i, frame) in frames.iter().enumerate() {
            log::info!("→ save slot {} (scale={})", ['A', 'B', 'C'][i], scale[i]);
            self.write(frame)?;
            thread::sleep(self.timings.slot_gap);
        }
        self.wait_for(MachineState::SlotsSaved, "slots saved")?;
        log::info!("presets stored to slots A/B/C — back to AUTO mode");
        self.write(&protocol::build_set_mode(false))?;
        thread::sleep(self.timings.mode_pause);
        Ok(())
    }
}

/// Lower-case hex (for logs).
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}
