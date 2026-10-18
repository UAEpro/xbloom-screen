//! Board definitions: which GPIO does what, and the panel timings.
//!
//! The firmware drives a parallel-RGB touch display on an ESP32-S3 with 16 MB flash and 8 MB
//! octal PSRAM (N16R8). One board is built in, chosen by a cargo feature (see `Cargo.toml`):
//!
//! - `board-sunton-8048s043` (default): Sunton **ESP32-8048S043C**, a 4.3" 800 × 480 IPS
//!   panel with GT911 touch and a PWM backlight.
//! - `board-waveshare-s3-touch-lcd-7`: Waveshare **ESP32-S3-Touch-LCD-7**, a 7" 800 × 480
//!   panel with GT911 touch. Its backlight, panel reset and touch reset are pins on a CH422G
//!   I/O expander (see `io.rs`), so the backlight is on or off, not dimmable.
//!
//! ⚠️ The pin maps and timings below follow the vendors' published board configurations and
//! have not been verified on hardware by this project yet. If the picture is shifted,
//! flickers or shows swapped colours, the timings and the data-line order are the place to
//! look; if touches land in the wrong spot, the touch flags.
//!
//! Another board is one more `Board` constant behind a cargo feature.

/// Parallel-RGB panel wiring and timing.
pub struct Panel {
    /// Pixel clock, Hz.
    pub pclk_hz: u32,
    /// Horizontal sync: pulse width, back porch, front porch (pixel clocks).
    pub hsync: (u32, u32, u32),
    /// Vertical sync: pulse width, back porch, front porch (lines).
    pub vsync: (u32, u32, u32),
    /// Data is clocked out on the falling PCLK edge.
    pub pclk_active_neg: bool,
    /// HSYNC, VSYNC, DE, PCLK GPIOs.
    pub hsync_gpio: i32,
    /// See `hsync_gpio`.
    pub vsync_gpio: i32,
    /// See `hsync_gpio`.
    pub de_gpio: i32,
    /// See `hsync_gpio`.
    pub pclk_gpio: i32,
    /// The 16 data lines, least significant bit first: B0–B4, G0–G5, R0–R4 (RGB565).
    pub data: [i32; 16],
}

/// A control line: a GPIO, or a pin on the board's I/O expander.
#[derive(Clone, Copy)]
#[allow(dead_code)] // each board uses some of the kinds
pub enum Line {
    /// ESP32-S3 GPIO number.
    Gpio(u8),
    /// CH422G pin (the board's EXIO number).
    Expander(u8),
}

/// How the backlight is driven.
#[derive(Clone, Copy)]
#[allow(dead_code)] // each board uses one of the kinds
pub enum Light {
    /// PWM on a GPIO: dimmable.
    Pwm(u8),
    /// An enable line: on or off. Any brightness above 0 % is full brightness, and the
    /// screensaver's dimming step has no effect.
    Switch(Line),
}

/// GT911 touch controller wiring.
pub struct Touch {
    /// I²C data.
    pub sda: u8,
    /// I²C clock.
    pub scl: u8,
    /// Controller reset, if wired.
    pub rst: Option<Line>,
    /// The INT GPIO, if the board wants it driven during the reset: held low, it selects
    /// I²C address 0x5D.
    pub int: Option<u8>,
    /// Swap the axes (panel mounted in portrait).
    pub swap_xy: bool,
    /// Mirror X.
    pub invert_x: bool,
    /// Mirror Y.
    pub invert_y: bool,
}

/// One supported board.
pub struct Board {
    /// Shown in the admin panel's "About".
    pub name: &'static str,
    /// Panel size, px.
    pub width: u32,
    /// See `width`.
    pub height: u32,
    /// Display.
    pub panel: Panel,
    /// Touch.
    pub touch: Touch,
    /// Backlight.
    pub backlight: Light,
    /// Panel reset, if wired.
    pub lcd_reset: Option<Line>,
    /// A CH422G I/O expander on the touch I²C bus, given as its IO0–IO7 levels at start-up
    /// (bit n = EXIOn).
    pub expander: Option<u8>,
}

/// Sunton ESP32-8048S043C (4.3", 800 × 480, GT911).
#[cfg(feature = "board-sunton-8048s043")]
pub const BOARD: Board = Board {
    name: "ESP32-8048S043C",
    width: 800,
    height: 480,
    panel: Panel {
        pclk_hz: 16_000_000,
        hsync: (4, 8, 8),
        vsync: (4, 8, 8),
        pclk_active_neg: true,
        hsync_gpio: 39,
        vsync_gpio: 41,
        de_gpio: 40,
        pclk_gpio: 42,
        //      B0 B1  B2 B3 B4  G0 G1 G2  G3  G4 G5  R0  R1  R2  R3  R4
        data: [8, 3, 46, 9, 1, 5, 6, 7, 15, 16, 4, 45, 48, 47, 21, 14],
    },
    touch: Touch {
        sda: 19,
        scl: 20,
        rst: Some(Line::Gpio(38)),
        int: None,
        swap_xy: false,
        invert_x: false,
        invert_y: false,
    },
    backlight: Light::Pwm(2),
    lcd_reset: None,
    expander: None,
};

/// Waveshare ESP32-S3-Touch-LCD-7 (7", 800 × 480, GT911, CH422G expander).
///
/// Expander pins: EXIO1 touch reset, EXIO2 backlight enable, EXIO3 panel reset, EXIO4 SD card
/// select (active low), EXIO5 USB/CAN select. EXIO5 must stay low: high switches GPIO19/20
/// from the USB-C port to the CAN transceiver.
#[cfg(feature = "board-waveshare-s3-touch-lcd-7")]
pub const BOARD: Board = Board {
    name: "Waveshare ESP32-S3-Touch-LCD-7",
    width: 800,
    height: 480,
    panel: Panel {
        pclk_hz: 16_000_000,
        hsync: (4, 8, 8),
        vsync: (4, 8, 8),
        pclk_active_neg: true,
        hsync_gpio: 46,
        vsync_gpio: 3,
        de_gpio: 5,
        pclk_gpio: 7,
        //      B0  B1  B2  B3  B4  G0 G1  G2  G3  G4  G5 R0 R1  R2  R3  R4
        data: [14, 38, 18, 17, 10, 39, 0, 45, 48, 47, 21, 1, 2, 42, 41, 40],
    },
    touch: Touch {
        sda: 8,
        scl: 9,
        rst: Some(Line::Expander(1)),
        int: Some(4),
        swap_xy: false,
        invert_x: false,
        invert_y: false,
    },
    backlight: Light::Switch(Line::Expander(2)),
    lcd_reset: Some(Line::Expander(3)),
    // Touch and panel out of reset, backlight off until the first frame, SD card deselected,
    // EXIO5 low (USB).
    expander: Some(0b0001_1010),
};

#[cfg(not(any(feature = "board-sunton-8048s043", feature = "board-waveshare-s3-touch-lcd-7")))]
compile_error!("select a board feature (see Cargo.toml), e.g. --features board-sunton-8048s043");

#[cfg(all(feature = "board-sunton-8048s043", feature = "board-waveshare-s3-touch-lcd-7"))]
compile_error!("select one board feature; for a board other than the default, add --no-default-features");
