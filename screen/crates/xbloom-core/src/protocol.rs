//! Byte-exact xBloom Studio command frames (host → machine, written to `ffe1`).
//!
//! A port of `xbloom_ble/protocol.py`; see the README's protocol reference for the full
//! story. Every builder here is checked byte-for-byte against the Python reference.
//!
//! ## Frame format
//!
//! ```text
//! 58 01 01 | CMD(u8) | SEQ(u8) | LEN(u16 LE) | 00 00 | PAYLOAD | CRC16(u16 LE)
//! ```
//!
//! `LEN` is the total frame length including header and CRC; the CRC is
//! [CRC-16/KERMIT](crate::crc::crc16_kermit) over everything before it. The dial-slot and
//! mode frames use a wider `58 01 02 | CMD(u16 LE) | LEN(u32 LE)` header.
//!
//! ## 🛑 Loading never brews
//!
//! [`build_load_frames`] returns exactly `[a4, a6, a8, pours]`. Commit (`0x42`), start
//! (`0x46`) and cancel (`0x47`) live only in [`build_commit`], [`build_start`] and
//! [`build_cancel`], which the driver sends only from an explicit start/cancel call.

use alloc::vec::Vec;

use crate::crc::crc16_kermit;
use crate::pyfmt::round_half_even;
use crate::recipe::Pattern;

/// Sequence byte used for the LOAD sequence (and the commit frame).
pub const LOAD_SEQ: u8 = 0x1F;
/// Sequence byte of the brew phase (start / cancel).
pub const BREW_SEQ: u8 = 0x9E;

/// Commit: armed → awaiting-confirm. A brew-control opcode — never part of a load.
pub const COMMIT_OPCODE: u8 = 0x42;
/// Start: the "go" that begins brewing. A brew-control opcode — never part of a load.
pub const START_OPCODE: u8 = 0x46;
/// Cancel: abort a committed / running brew.
pub const CANCEL_OPCODE: u8 = 0x47;

/// Session start (first LOAD frame, also sent on connect).
pub const SESSION_OPCODE: u8 = 0xA4;
/// Dose frame.
pub const DOSE_OPCODE: u8 = 0xA6;
/// Stage-temperatures frame.
pub const TEMPS_OPCODE: u8 = 0xA8;
/// Status / handshake query sent after `a4` before staging.
pub const STATUS_QUERY_OPCODE: u8 = 0x56;
/// Pours frame opcode when the machine grinds.
pub const POURS_CMD_GRIND: u8 = 0x41;
/// Pours frame opcode when the grinder is off (no-grind / pre-ground).
pub const POURS_CMD_NO_GRIND: u8 = 0x44;

/// Recipe-level grind meaning "don't grind" (brew pre-ground).
pub const NO_GRIND: i32 = 0;
/// The wire byte the machine reads as "skip the grinder".
pub const NO_GRIND_WIRE: u8 = 0xFE;

/// Easy-Mode dial-slot write command (`0x2CF6`).
pub const CMD_SAVE_SLOT: u16 = 0x2CF6;
/// Pro / Auto mode switch command (`0x2CF7`).
pub const CMD_SET_MODE: u16 = 0x2CF7;
/// Slot flags: store the preset with the on-brew scale enabled.
pub const SLOT_FLAG_SCALE_ON: u8 = 0x12;
/// Slot flags: store the preset with the on-brew scale disabled.
pub const SLOT_FLAG_SCALE_OFF: u8 = 0x02;
const MODE_PRO_PAYLOAD: [u8; 4] = [0x00, 0x00, 0x00, 0x00];
const MODE_AUTO_PAYLOAD: [u8; 4] = [0x91, 0x32, 0x78, 0x56];

/// Errors from encoding a recipe onto the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// A pour's `(pattern, agitation)` combination has no known wire code.
    UnknownPattern {
        /// 1-based pour index.
        pour: usize,
    },
    /// The encoded pours body does not fit the frame's one-byte length field.
    PoursTooLong {
        /// Encoded body length in bytes.
        len: usize,
    },
    /// A dial-slot index other than 0 (A), 1 (B) or 2 (C).
    BadSlot(u8),
}

impl core::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownPattern { pour } => {
                write!(f, "pour #{pour}: pattern/agitation has no wire encoding")
            }
            Self::PoursTooLong { len } => write!(
                f,
                "the pours encode to {len} bytes but the frame holds at most 255 — \
                 use fewer or smaller pours"
            ),
            Self::BadSlot(s) => write!(f, "slot must be 0 (A), 1 (B) or 2 (C); got {s}"),
        }
    }
}

/// One pour as the protocol layer sees it (unvalidated; values are masked like the reference).
#[derive(Debug, Clone, PartialEq)]
pub struct WirePour {
    /// Pour volume, ml.
    pub ml: i32,
    /// Water temperature, °C.
    pub temp: i32,
    /// Pour pattern.
    pub pattern: Pattern,
    /// Agitation (only valid with spiral).
    pub agitation: bool,
    /// Pause after the pour, seconds.
    pub pause: i32,
    /// Agitation speed, RPM (only carried on the first pour).
    pub rpm: i32,
    /// Flow rate, ml/s.
    pub flow: f64,
}

/// A recipe as the protocol layer sees it — the shape of the reference's protocol dict.
#[derive(Debug, Clone, PartialEq)]
pub struct WireRecipe {
    /// Dose, grams.
    pub dose: i32,
    /// Grind setting 1–80, or [`NO_GRIND`].
    pub grind: i32,
    /// Machine stage set-points (default 110 / 90 °C).
    pub stage_temps: (f64, f64),
    /// The pours, in order.
    pub pours: Vec<WirePour>,
    /// Explicit override for the pours frame's trailing ratio byte (edge cases only).
    pub tail: Option<u8>,
    /// Sequence byte (defaults to [`LOAD_SEQ`]).
    pub seq: u8,
}

/// `(pattern, agitation) → (pat_byte, agit_byte)`; `None` for an unknown combination.
pub fn pattern_codes(pattern: &Pattern, agitation: bool) -> Option<(u8, u8)> {
    match (pattern, agitation) {
        (Pattern::Spiral, true) => Some((0x02, 0x02)),
        (Pattern::Spiral, false) => Some((0x02, 0x00)),
        (Pattern::Ring, false) => Some((0x01, 0x00)),
        (Pattern::Center, false) => Some((0x00, 0x01)),
        _ => None,
    }
}

/// Build a complete `ffe1` command frame: `58 01 01 | cmd | seq | len | 00 00 | payload | crc`.
pub fn xbloom_frame(cmd: u8, seq: u8, payload: &[u8]) -> Vec<u8> {
    let total = 9 + payload.len() + 2;
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&[0x58, 0x01, 0x01, cmd, seq]);
    frame.extend_from_slice(&(total as u16).to_le_bytes());
    frame.extend_from_slice(&[0x00, 0x00]);
    frame.extend_from_slice(payload);
    let crc = crc16_kermit(&frame);
    frame.extend_from_slice(&crc.to_le_bytes());
    frame
}

/// Wide `58 01 02` frame used by the dial-slot and mode commands (u16 command, u32 length).
fn wide_frame(cmd: u16, payload: &[u8]) -> Vec<u8> {
    let total = 9 + payload.len() + 2;
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&[0x58, 0x01, 0x02]);
    frame.extend_from_slice(&cmd.to_le_bytes());
    frame.extend_from_slice(&(total as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    let crc = crc16_kermit(&frame);
    frame.extend_from_slice(&crc.to_le_bytes());
    frame
}

/// `0xa4` session-start payload (observed constant).
pub fn build_a4() -> [u8; 9] {
    [0x01, 0xB9, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00]
}

/// `0xa6` dose payload: dose grams as `u8` at offset 9 of a 13-byte payload.
pub fn build_a6(dose_g: i32) -> [u8; 13] {
    let mut pl = [0u8; 13];
    pl[0] = 0x01;
    pl[9] = (dose_g & 0xFF) as u8;
    pl
}

/// `0xa8` stage-temps payload: `01` + f32 LE temp1 + f32 LE temp2.
pub fn build_a8(temp1: f64, temp2: f64) -> [u8; 9] {
    let mut pl = [0u8; 9];
    pl[0] = 0x01;
    pl[1..5].copy_from_slice(&(temp1 as f32).to_le_bytes());
    pl[5..9].copy_from_slice(&(temp2 as f32).to_le_bytes());
    pl
}

/// Append one logical pour's segments: `[ml, temp, pat, agit, negpause, 00, rpm, flow×10]`.
///
/// A pour above 127 ml is split into 127-ml 4-byte lead segments (`[ml, temp, pat, agit]`)
/// followed by an 8-byte remainder carrying the pause / rpm / flow fields.
fn push_pour_segments(out: &mut Vec<u8>, p: &WirePour, rpm: i32, idx: usize) -> Result<(), ProtocolError> {
    let (pat, agit) = pattern_codes(&p.pattern, p.agitation).ok_or(ProtocolError::UnknownPattern { pour: idx + 1 })?;
    let temp = (p.temp & 0xFF) as u8;
    let flow10 = (round_half_even(p.flow * 10.0) as i64 & 0xFF) as u8;
    let negpause = ((256 - p.pause) & 0xFF) as u8;
    let mut remaining = p.ml;
    while remaining > 127 {
        out.extend_from_slice(&[127, temp, pat, agit]);
        remaining -= 127;
    }
    out.extend_from_slice(&[(remaining & 0xFF) as u8, temp, pat, agit, negpause, 0x00, (rpm & 0xFF) as u8, flow10]);
    Ok(())
}

/// Map a recipe grind to its wire byte: `0` (no-grind) → `0xFE`, else the grind.
pub fn grind_byte(grind: i32) -> u8 {
    if grind == NO_GRIND {
        NO_GRIND_WIRE
    } else {
        (grind & 0xFF) as u8
    }
}

/// The pours body: `01 | LEN(u8) | <segments> | grind | tail`.
///
/// RPM is carried only on the first pour — the machine zeroes it on later pours (verified
/// byte-for-byte against the vendor app's captures).
pub fn build_41(pours: &[WirePour], grind: i32, tail: u8) -> Result<Vec<u8>, ProtocolError> {
    let mut segs = Vec::with_capacity(pours.len() * 8);
    for (i, p) in pours.iter().enumerate() {
        push_pour_segments(&mut segs, p, if i == 0 { p.rpm } else { 0 }, i)?;
    }
    if segs.len() > 0xFF {
        return Err(ProtocolError::PoursTooLong { len: segs.len() });
    }
    let mut body = Vec::with_capacity(segs.len() + 4);
    body.push(0x01);
    body.push(segs.len() as u8);
    body.extend_from_slice(&segs);
    body.push(grind_byte(grind));
    body.push(tail);
    Ok(body)
}

/// The pours frame's trailing byte: the brew **ratio × 10** (water : coffee).
///
/// e.g. 1:10 → `0x64`, 1:15 → `0x96`, 1:16 → `0xa0`. The machine validates it against
/// Σ(pour ml) / dose and rejects a load whose ratio byte doesn't match.
pub fn ratio_byte(recipe: &WireRecipe) -> u8 {
    if let Some(tail) = recipe.tail {
        return tail;
    }
    let total: i64 = recipe.pours.iter().map(|p| i64::from(p.ml)).sum();
    if recipe.dose == 0 {
        return 0xA0;
    }
    let ratio10 = round_half_even(total as f64 / f64::from(recipe.dose) * 10.0) as i64;
    (ratio10 & 0xFF) as u8
}

/// The pours-frame opcode for a recipe: `0x41` (grinding) or `0x44` (no-grind).
pub fn pours_opcode(grind: i32) -> u8 {
    if grind == NO_GRIND {
        POURS_CMD_NO_GRIND
    } else {
        POURS_CMD_GRIND
    }
}

/// Build the ordered LOAD frames for a recipe: exactly `[a4, a6, a8, pours]`.
///
/// 🛑 This is the load path and it **never** contains commit (`0x42`), start (`0x46`) or
/// cancel (`0x47`): loading only arms the machine, so a load can never brew. The check at
/// the end is a hard guard, kept even though the builders above cannot produce those
/// opcodes — it is the last line of defence if someone edits this function.
pub fn build_load_frames(recipe: &WireRecipe) -> Result<[Vec<u8>; 4], ProtocolError> {
    let seq = recipe.seq;
    let pours = build_41(&recipe.pours, recipe.grind, ratio_byte(recipe))?;
    let frames = [
        xbloom_frame(SESSION_OPCODE, seq, &build_a4()),
        xbloom_frame(DOSE_OPCODE, seq, &build_a6(recipe.dose)),
        xbloom_frame(TEMPS_OPCODE, seq, &build_a8(recipe.stage_temps.0, recipe.stage_temps.1)),
        xbloom_frame(pours_opcode(recipe.grind), seq, &pours),
    ];
    for fr in &frames {
        assert!(
            !matches!(fr[3], COMMIT_OPCODE | START_OPCODE | CANCEL_OPCODE),
            "load frames must never contain a brew-start/cancel opcode"
        );
    }
    Ok(frames)
}

/// The `0x42` **commit** frame (armed → awaiting-confirm, ~99 s countdown).
///
/// ⚠️ A brew-control frame. Emit it only in response to an explicit start action.
pub fn build_commit() -> Vec<u8> {
    xbloom_frame(COMMIT_OPCODE, LOAD_SEQ, &[0x01])
}

/// The `0x46` **start** frame — the "go" that begins brewing.
///
/// ⚠️ This physically dispenses near-boiling water.
pub fn build_start() -> Vec<u8> {
    xbloom_frame(START_OPCODE, BREW_SEQ, &[0x01])
}

/// The `0x47` **cancel** frame — abort a committed / running brew.
pub fn build_cancel() -> Vec<u8> {
    xbloom_frame(CANCEL_OPCODE, BREW_SEQ, &[0x01])
}

/// The `0xa4` session-start frame the app sends once right after connecting.
pub fn build_session_start() -> Vec<u8> {
    xbloom_frame(SESSION_OPCODE, LOAD_SEQ, &build_a4())
}

/// The `0x56` status / handshake frame sent after `a4` before staging a recipe.
pub fn build_status_query() -> Vec<u8> {
    xbloom_frame(STATUS_QUERY_OPCODE, LOAD_SEQ, &[0x01])
}

/// Build the frame that writes `recipe` to Easy-Mode dial `slot` (0=A, 1=B, 2=C).
///
/// `58 01 02 | f6 2c | LEN(u32) | 01 | slot | flags | <0x41 body minus its leading 01> | CRC`.
/// A preset write only — it never brews. The machine stores presets only once all three
/// slots arrive in one batch (see [`crate::driver::Machine::save_slots`]).
pub fn build_save_slot(recipe: &WireRecipe, slot: u8, scale: bool) -> Result<Vec<u8>, ProtocolError> {
    if slot > 2 {
        return Err(ProtocolError::BadSlot(slot));
    }
    let blob = build_41(&recipe.pours, recipe.grind, ratio_byte(recipe))?;
    let flags = if scale { SLOT_FLAG_SCALE_ON } else { SLOT_FLAG_SCALE_OFF };
    let mut payload = Vec::with_capacity(blob.len() + 2);
    payload.extend_from_slice(&[0x01, slot, flags]);
    payload.extend_from_slice(&blob[1..]);
    Ok(wide_frame(CMD_SAVE_SLOT, &payload))
}

/// Build the frame that switches the machine between PRO (`true`) and AUTO (`false`) mode.
///
/// Slot writes are only accepted in PRO mode (status `0x01` idle); AUTO is the on-machine
/// A/B/C selector. A display-mode switch only — it never brews.
pub fn build_set_mode(pro: bool) -> Vec<u8> {
    let mut payload = [0u8; 5];
    payload[0] = 0x01;
    payload[1..].copy_from_slice(if pro { &MODE_PRO_PAYLOAD } else { &MODE_AUTO_PAYLOAD });
    wide_frame(CMD_SET_MODE, &payload)
}

/// A parsed host → machine frame (used by the simulator and diagnostics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand<'a> {
    /// The opcode: one byte for `58 01 01` frames, the u16 for wide `58 01 02` frames.
    pub cmd: u16,
    /// Sequence byte (`0` for wide frames, which carry none).
    pub seq: u8,
    /// The payload between header and CRC.
    pub payload: &'a [u8],
}

/// Parse and CRC-check a host → machine command frame. Returns `None` if it is malformed.
pub fn parse_command(frame: &[u8]) -> Option<ParsedCommand<'_>> {
    if frame.len() < 11 || frame[0] != 0x58 || frame[1] != 0x01 {
        return None;
    }
    let body = &frame[..frame.len() - 2];
    let crc = u16::from_le_bytes([frame[frame.len() - 2], frame[frame.len() - 1]]);
    if crc16_kermit(body) != crc {
        return None;
    }
    match frame[2] {
        0x01 => {
            let len = usize::from(u16::from_le_bytes([frame[5], frame[6]]));
            (len == frame.len()).then(|| ParsedCommand {
                cmd: u16::from(frame[3]),
                seq: frame[4],
                payload: &frame[9..frame.len() - 2],
            })
        }
        0x02 => {
            let len = u32::from_le_bytes([frame[5], frame[6], frame[7], frame[8]]) as usize;
            (len == frame.len()).then(|| ParsedCommand {
                cmd: u16::from_le_bytes([frame[3], frame[4]]),
                seq: 0,
                payload: &frame[9..frame.len() - 2],
            })
        }
        _ => None,
    }
}

/// Decode a pours body (`01 | LEN | segments | grind | tail`) back into pours.
///
/// The inverse of [`build_41`], used by the simulator to brew what it was sent. Lead
/// segments (4 bytes) are told apart from full segments (8 bytes) by the full segment's
/// constant zero at offset 5 — a lead is followed by the next segment's temperature there,
/// which is never zero for a valid recipe. Returns `(pours, grind_byte, tail)`.
pub fn decode_pours(body: &[u8]) -> Option<(Vec<WirePour>, u8, u8)> {
    if body.len() < 4 || body[0] != 0x01 {
        return None;
    }
    let len = usize::from(body[1]);
    if body.len() != len + 4 {
        return None;
    }
    let segs = &body[2..2 + len];
    let mut pours = Vec::new();
    let mut lead_ml = 0;
    let mut i = 0;
    while i < segs.len() {
        let full = i + 8 <= segs.len() && segs[i + 5] == 0x00;
        if !full {
            if i + 4 > segs.len() {
                return None;
            }
            lead_ml += i32::from(segs[i]);
            i += 4;
            continue;
        }
        let s = &segs[i..i + 8];
        let (pattern, agitation) = match (s[2], s[3]) {
            (0x02, 0x02) => (Pattern::Spiral, true),
            (0x02, 0x00) => (Pattern::Spiral, false),
            (0x01, 0x00) => (Pattern::Ring, false),
            (0x00, 0x01) => (Pattern::Center, false),
            _ => return None,
        };
        pours.push(WirePour {
            ml: lead_ml + i32::from(s[0]),
            temp: i32::from(s[1]),
            pattern,
            agitation,
            pause: (256 - i32::from(s[4])) & 0xFF,
            rpm: i32::from(s[6]),
            flow: f64::from(s[7]) / 10.0,
        });
        lead_ml = 0;
        i += 8;
    }
    Some((pours, body[2 + len], body[3 + len]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn pour(ml: i32, pattern: Pattern, agitation: bool, pause: i32, rpm: i32) -> WirePour {
        WirePour { ml, temp: 92, pattern, agitation, pause, rpm, flow: 3.0 }
    }

    fn simple() -> WireRecipe {
        WireRecipe {
            dose: 16,
            grind: 62,
            stage_temps: (110.0, 90.0),
            pours: vec![pour(35, Pattern::Spiral, false, 40, 100), pour(115, Pattern::Spiral, false, 5, 100)],
            tail: None,
            seq: LOAD_SEQ,
        }
    }

    /// 🛑 Safety: LOAD frames never carry a brew-start/cancel opcode — loading can't brew.
    #[test]
    fn load_frames_are_load_only() {
        let mut no_grind = simple();
        no_grind.grind = 0;
        for r in [simple(), no_grind] {
            for frame in build_load_frames(&r).unwrap() {
                assert!(!matches!(frame[3], COMMIT_OPCODE | START_OPCODE | CANCEL_OPCODE));
            }
        }
    }

    /// 🛑 Safety: the four LOAD frames are exactly a4, a6, a8, 41 in order.
    #[test]
    fn load_frames_opcode_order() {
        let ops: Vec<u8> = build_load_frames(&simple()).unwrap().iter().map(|f| f[3]).collect();
        assert_eq!(ops, vec![0xA4, 0xA6, 0xA8, 0x41]);
    }

    #[test]
    fn commit_start_cancel_match_capture() {
        let hex = |v: Vec<u8>| v.iter().map(|b| alloc::format!("{b:02x}")).collect::<alloc::string::String>();
        assert_eq!(hex(build_commit()), "580101421f0c000000017fcf");
        assert_eq!(hex(build_start()), "580101469e0c0000000180a1");
        assert_eq!(hex(build_cancel()), "580101479e0c00000001553e");
    }

    #[test]
    fn decode_round_trips_including_splits() {
        let r = WireRecipe {
            pours: vec![
                pour(254, Pattern::Spiral, true, 30, 90),
                pour(300, Pattern::Ring, false, 0, 0),
                pour(60, Pattern::Center, false, 10, 0),
                pour(127, Pattern::Spiral, false, 5, 0),
            ],
            ..simple()
        };
        let body = build_41(&r.pours, r.grind, ratio_byte(&r)).unwrap();
        let (pours, grind, _) = decode_pours(&body).unwrap();
        assert_eq!(grind, 62);
        assert_eq!(pours, r.pours);
    }

    #[test]
    fn parse_command_checks_crc_and_length() {
        let f = build_commit();
        let p = parse_command(&f).unwrap();
        assert_eq!((p.cmd, p.seq, p.payload), (0x42, 0x1F, &[0x01][..]));
        let mut bad = f.clone();
        bad[9] ^= 0xFF;
        assert!(parse_command(&bad).is_none());
        let slot = build_save_slot(&simple(), 1, true).unwrap();
        assert_eq!(parse_command(&slot).unwrap().cmd, CMD_SAVE_SLOT);
    }

    #[test]
    fn oversize_pours_are_rejected_not_wrapped() {
        let r = WireRecipe { pours: vec![pour(4000, Pattern::Spiral, false, 5, 100); 3], ..simple() };
        assert!(matches!(build_load_frames(&r), Err(ProtocolError::PoursTooLong { .. })));
    }
}
