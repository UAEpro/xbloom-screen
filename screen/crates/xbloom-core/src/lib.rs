//! # xbloom-core
//!
//! The shared heart of the xBloom screen: a byte-exact Rust port of the reverse-engineered
//! xBloom Studio Bluetooth LE protocol from the `xbloom_ble` Python package in this repo,
//! plus the pieces every front-end needs.
//!
//! | Module        | What it is | `no_std` |
//! |---------------|------------|----------|
//! | [`crc`]       | CRC-16/KERMIT, the frame checksum | ✅ |
//! | [`protocol`]  | Command frame builders: LOAD, commit/start/cancel, dial slots, mode | ✅ |
//! | [`telemetry`] | Decoder for `ffe2` notifications (state machine + live scale weights) | ✅ |
//! | [`recipe`]    | The recipe model, the `xbloom_ble`-compatible schema, and validation | ✅ |
//! | [`brew`]      | The brew monitor: phases, completion detection, pour tracking | ✅ |
//! | [`driver`]    | The blocking machine driver over a pluggable BLE [`driver::Transport`] | `std` |
//! | [`sim`]       | A simulated machine that speaks the real wire protocol | `std` |
//!
//! Everything is checked against golden vectors generated from the Python reference
//! (`screen/tools/gen_golden.py`), so frames match byte-for-byte.
//!
//! ## 🛑 Safety invariant
//!
//! **Loading a recipe only arms the machine; it never starts a brew.**
//! [`protocol::build_load_frames`] returns exactly the four LOAD frames and refuses to emit a
//! commit/start/cancel opcode. Starting is always a separate, explicit
//! [`driver::Machine::start`] call, which a UI must put behind a deliberate gesture.
#![cfg_attr(not(feature = "std"), no_std)]
#![warn(missing_docs)]

extern crate alloc;

pub mod brew;
pub mod crc;
pub mod protocol;
mod pyfmt;
pub mod recipe;
pub mod telemetry;

#[cfg(feature = "std")]
pub mod driver;
#[cfg(feature = "std")]
pub mod sim;

pub use recipe::{Pattern, Pour, Recipe, RecipeError};
pub use telemetry::{parse_notification, MachineState, StatusEvent};
