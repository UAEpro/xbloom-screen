//! Decode xBloom Studio status notifications (machine → host, the `ffe2` characteristic).
//!
//! A port of `xbloom_ble/telemetry.py`. Notifications use their own frame shape:
//!
//! ```text
//! 58 02 07 | TYPE(u8) | SUB(u8) | LEN(u32 LE) | 0xc1 | PAYLOAD | CRC16(u16 LE)
//! ```
//!
//! * `TYPE == 0x57` — a **status** frame; the byte after `0xc1` is the machine state.
//! * `TYPE == 0x4b` — live **water** weight, float32 LE in milligrams (~10×/s).
//! * `TYPE == 0x15` — live **coffee / cup** weight, float32 LE in grams (~10×/s).
//! * anything else — a command echo / ACK whose `TYPE` is the acked command byte.
//!
//! Observed on firmware V12.0D.500: after commit the machine goes `awaiting_confirm (0x1e) →
//! starting (0x22)`, then grinds **silently** for ~20 s (scale stream only) before reporting
//! the pour as `0x10`. Consumers must not treat that gap as a stalled brew.

use alloc::format;
use alloc::string::String;

/// TYPE byte of a status frame.
pub const STATUS_TYPE: u8 = 0x57;
/// TYPE byte of the water-weight stream (float32 LE milligrams).
pub const WATER_TYPE: u8 = 0x4B;
/// TYPE byte of the coffee / cup-weight stream (float32 LE grams).
pub const COFFEE_TYPE: u8 = 0x15;
/// The payload marker that follows the fixed 9-byte notification header.
pub const STATE_MARKER: u8 = 0xC1;

/// A machine state (the byte inside a `0x57` status frame).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MachineState {
    /// `0x01` — idle / ready (also reached at brew end once the cup is lifted).
    Idle,
    /// `0x0c` — refused: no water.
    NoWater,
    /// `0x0f` — refused: wants beans (the machine waits here).
    NoBeans,
    /// `0x10`, `0x23`, `0x3b` — brew in progress.
    Brewing(u8),
    /// `0x1d` — recipe being received.
    Loading,
    /// `0x1f` — recipe loaded and armed, awaiting approval.
    Armed,
    /// `0x1e` — waiting for the human to confirm on the device.
    AwaitingConfirm,
    /// `0x22` — post-confirm: grinding / spinning up.
    Starting,
    /// `0x24` — "coffee ready" beep: the brew is done, cup still on the scale.
    Ready,
    /// `0x41` — complete (also the Auto-mode A/B/C selector).
    Complete,
    /// `0x43` — dial-slot batch being stored.
    SavingSlots,
    /// `0x25` — dial slots stored OK (then → idle).
    SlotsSaved,
    /// Any other state byte.
    Unknown(u8),
}

impl MachineState {
    /// Decode a state byte.
    pub fn from_byte(b: u8) -> Self {
        match b {
            0x01 => Self::Idle,
            0x0C => Self::NoWater,
            0x0F => Self::NoBeans,
            0x10 | 0x23 | 0x3B => Self::Brewing(b),
            0x1D => Self::Loading,
            0x1F => Self::Armed,
            0x1E => Self::AwaitingConfirm,
            0x22 => Self::Starting,
            0x24 => Self::Ready,
            0x41 => Self::Complete,
            0x43 => Self::SavingSlots,
            0x25 => Self::SlotsSaved,
            other => Self::Unknown(other),
        }
    }

    /// The raw state byte.
    pub fn byte(self) -> u8 {
        match self {
            Self::Idle => 0x01,
            Self::NoWater => 0x0C,
            Self::NoBeans => 0x0F,
            Self::Brewing(b) | Self::Unknown(b) => b,
            Self::Loading => 0x1D,
            Self::Armed => 0x1F,
            Self::AwaitingConfirm => 0x1E,
            Self::Starting => 0x22,
            Self::Ready => 0x24,
            Self::Complete => 0x41,
            Self::SavingSlots => 0x43,
            Self::SlotsSaved => 0x25,
        }
    }

    /// The reference's state name (`"idle"`, `"armed"`, `"unknown_0x77"`, …).
    pub fn name(self) -> String {
        let s = match self {
            Self::Idle => "idle",
            Self::NoWater => "no_water",
            Self::NoBeans => "no_beans",
            Self::Brewing(_) => "brewing",
            Self::Loading => "loading",
            Self::Armed => "armed",
            Self::AwaitingConfirm => "awaiting_confirm",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Complete => "complete",
            Self::SavingSlots => "saving_slots",
            Self::SlotsSaved => "slots_saved",
            Self::Unknown(b) => return format!("unknown_0x{b:02x}"),
        };
        String::from(s)
    }

    /// States that mean the brew is over: ready (`0x24`), complete (`0x41`), idle (`0x01`).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Ready | Self::Complete | Self::Idle)
    }
}

/// What kind of notification a [`StatusEvent`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// A `0x57` status frame carrying a machine state.
    Status(MachineState),
    /// A live scale frame (water or coffee weight).
    Scale,
    /// A command echo / ACK (or an undecoded progress frame) with this TYPE byte.
    Ack(u8),
}

/// A decoded notification.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusEvent {
    /// What the frame was.
    pub kind: EventKind,
    /// Live water weight in grams (water-stream frames only; `None` for idle noise).
    pub water_g: Option<f32>,
    /// Live coffee / cup weight in grams (coffee-stream frames only).
    pub coffee_g: Option<f32>,
}

impl StatusEvent {
    /// A status event for `state` (used by the simulator and tests).
    pub fn status(state: MachineState) -> Self {
        Self { kind: EventKind::Status(state), water_g: None, coffee_g: None }
    }

    /// The machine state, for status frames.
    pub fn state(&self) -> Option<MachineState> {
        match self.kind {
            EventKind::Status(s) => Some(s),
            _ => None,
        }
    }

    /// The reference's `state_name` (`"armed"`, `"scale"`, `"ack_0x41"`, …).
    pub fn state_name(&self) -> String {
        match self.kind {
            EventKind::Status(s) => s.name(),
            EventKind::Scale => String::from("scale"),
            EventKind::Ack(t) => format!("ack_0x{t:02x}"),
        }
    }

    /// True for an ACK of `cmd` (the notification whose TYPE byte equals the command).
    pub fn is_ack_of(&self, cmd: u8) -> bool {
        self.kind == EventKind::Ack(cmd)
    }

    /// Heartbeat states (`0x15`/`0x4b` reported as *states*) — ignored by consumers.
    pub fn is_heartbeat(&self) -> bool {
        matches!(self.state(), Some(MachineState::Unknown(0x15 | 0x4B)))
    }

    /// True once the brew is over (see [`MachineState::is_terminal`]).
    pub fn is_terminal(&self) -> bool {
        self.state().is_some_and(MachineState::is_terminal)
    }
}

/// Offset of the `0xc1` payload marker: normally 9 (fixed header), else searched from 5.
fn marker_idx(data: &[u8]) -> Option<usize> {
    if data.len() > 9 && data[9] == STATE_MARKER {
        return Some(9);
    }
    data.iter().skip(5).position(|&b| b == STATE_MARKER).map(|i| i + 5)
}

/// Decode a scale frame's float32 LE weight after the marker, in grams.
///
/// Returns `None` for implausible readings (NaN, negative, > 2 kg) — the scale drifts and
/// reads noise when idle or untared.
fn decode_scale_grams(data: &[u8], scale: f64) -> Option<f32> {
    let m = marker_idx(data)?;
    if m + 5 > data.len() {
        return None;
    }
    let raw = f32::from_le_bytes([data[m + 1], data[m + 2], data[m + 3], data[m + 4]]);
    let grams = f64::from(raw) * scale;
    if grams.is_nan() || !(0.0..=2000.0).contains(&grams) {
        return None;
    }
    Some((libm::round(grams * 100.0) / 100.0) as f32)
}

/// Decode a raw `ffe2` notification. Returns `None` for unrecognisable frames.
pub fn parse_notification(data: &[u8]) -> Option<StatusEvent> {
    if data.len() < 10 || data[0] != 0x58 {
        return None;
    }
    let ftype = data[3];
    if ftype == WATER_TYPE {
        let g = decode_scale_grams(data, 0.001);
        return Some(StatusEvent { kind: EventKind::Scale, water_g: g, coffee_g: None });
    }
    if ftype == COFFEE_TYPE {
        let g = decode_scale_grams(data, 1.0);
        return Some(StatusEvent { kind: EventKind::Scale, water_g: None, coffee_g: g });
    }
    if ftype == STATUS_TYPE {
        if let Some(m) = marker_idx(data) {
            let end = data.len() - 2;
            if m + 1 < end {
                let state = MachineState::from_byte(data[m + 1]);
                return Some(StatusEvent::status(state));
            }
        }
    }
    Some(StatusEvent { kind: EventKind::Ack(ftype), water_g: None, coffee_g: None })
}

/// Build a machine → host notification frame (the simulator speaks the real format).
pub fn build_notification(ftype: u8, sub: u8, payload: &[u8]) -> alloc::vec::Vec<u8> {
    let total = 10 + payload.len() + 2;
    let mut f = alloc::vec::Vec::with_capacity(total);
    f.extend_from_slice(&[0x58, 0x02, 0x07, ftype, sub]);
    f.extend_from_slice(&(total as u32).to_le_bytes());
    f.push(STATE_MARKER);
    f.extend_from_slice(payload);
    let crc = crate::crc::crc16_kermit(&f);
    f.extend_from_slice(&crc.to_le_bytes());
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_and_scale_frames_decode() {
        let armed = build_notification(STATUS_TYPE, 0, &[0x1F, 0, 0]);
        assert_eq!(parse_notification(&armed).unwrap().state(), Some(MachineState::Armed));
        let water = build_notification(WATER_TYPE, 0, &45_000f32.to_le_bytes());
        assert_eq!(parse_notification(&water).unwrap().water_g, Some(45.0));
        let ack = build_notification(0xA6, 0, &[1]);
        assert!(parse_notification(&ack).unwrap().is_ack_of(0xA6));
    }
}
