//! Board definitions: which GPIO does what, and the panel timings.
//!
//! The firmware drives a parallel-RGB touch display. The default board is the Sunton
//! **ESP32-8048S043C**: an ESP32-S3 (N16R8 — 16 MB flash, 8 MB octal PSRAM) with a 4.3"
//! 800 × 480 RGB565 IPS panel, a GT911 capacitive touch controller and a USB-UART bridge.
//!
//! ⚠️ The pin map and timings below follow the vendor's published examples and have not been
//! verified on hardware by this project yet. If the picture is shifted, flickers or shows
//! swapped colours, the timings and the data-line order are the place to look; if touches
//! land in the wrong spot, the touch flags.
//!
//! Another board is one more `Board` constant behind a cargo feature (see `Cargo.toml`).

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

/// GT911 touch controller wiring.
pub struct Touch {
    /// I²C data.
    pub sda: u8,
    /// I²C clock.
    pub scl: u8,
    /// Controller reset, if wired.
    pub rst: Option<u8>,
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
    /// Backlight enable / PWM GPIO.
    pub backlight: u8,
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
    touch: Touch { sda: 19, scl: 20, rst: Some(38), swap_xy: false, invert_x: false, invert_y: false },
    backlight: 2,
};

#[cfg(not(feature = "board-sunton-8048s043"))]
compile_error!("select a board feature (see Cargo.toml), e.g. --features board-sunton-8048s043");
