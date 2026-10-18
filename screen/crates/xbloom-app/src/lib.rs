//! # xbloom-app
//!
//! The application layer of the xBloom screen, shared by the Raspberry Pi app and the ESP32
//! firmware. Platform code supplies a BLE [`xbloom_core::driver::Transport`], a [`Platform`]
//! (backlight, restart, Wi-Fi) and an HTTP server that forwards to [`api::handle`]; the UI
//! subscribes to the [`App`]'s events.
//!
//! | Module | What it is |
//! |---|---|
//! | [`app`] | The [`App`]: shared state, the operations both front-ends call, the event bus |
//! | [`controller`] | The machine thread: connection, brews, dial presets, reconnects |
//! | [`settings`] | Appearance, screensaver, lock, machine, Wi-Fi |
//! | [`auth`] | PBKDF2 PINs, brute-force throttling, admin web sessions |
//! | [`recipes`] | The recipe library (`xbloom` CLI-compatible YAML/JSON import) |
//! | [`history`] | Completed brews with their curves |
//! | [`presets`] | The machine's A/B/C dial presets |
//! | [`media`] | Screensaver videos (`XBV1` Motion-JPEG) and photos |
//! | [`api`] | The admin REST API + the embedded admin panel |
//!
//! 🛑 Safety: [`App::brew`] with `start: false` only loads (the machine arms and a human
//! approves on it); `start: true` is the explicit remote start, refused when the admin turned
//! it off. The HTTP API has no way to start a brew.

pub mod api;
pub mod app;
pub mod auth;
pub mod controller;
pub mod history;
pub mod media;
pub mod presets;
pub mod recipes;
pub mod settings;
pub mod util;

pub use app::{App, Data, Event, Hotspot, PinPurpose, Platform, PlatformInfo, ToastKind, VideoProfile};
pub use controller::{BrewStatus, Link, MachineStatus};
