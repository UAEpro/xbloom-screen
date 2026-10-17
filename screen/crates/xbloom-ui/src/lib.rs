//! # xbloom-ui
//!
//! The xBloom touchscreen UI, written in [Slint](https://slint.dev) and shared by the Raspberry
//! Pi app and the ESP32-S3 firmware: the `.slint` files in `ui/` describe every screen, and
//! [`bind`] connects them to an [`xbloom_app::App`].
//!
//! The UI is drawn with Slint's software renderer on both targets, so a Pi shows exactly what an
//! ESP32 shows (and what [`screenshot`] renders offscreen).
//!
//! Licensing note: Slint is used under its GPLv3 option for embedded devices unless you hold a
//! Slint commercial licence; this crate's own source is MIT.

slint::include_modules!();

pub mod bind;
pub mod clock;
pub mod format;
pub mod qr;
pub mod saver;
#[cfg(feature = "screenshot")]
pub mod screenshot;

pub use bind::bind;
