//! The board's control lines: plain GPIOs, or pins on a CH422G I/O expander.
//!
//! Some boards spend nearly every GPIO on the 16-bit RGB panel and move the slow control
//! lines (backlight enable, panel and touch reset, SD card select) to a CH422G on the touch
//! controller's I²C bus. The CH422G has no registers: each command is a one-byte write to an
//! I²C address of its own. `0x24` sets the mode (bit 0: IO0–IO7 are outputs) and `0x38` sets
//! the IO0–IO7 output levels. The levels can't be read back, so they are kept here.

use std::sync::{Arc, Mutex};

use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::hal::i2c::I2cDriver;
use esp_idf_svc::sys::{self, EspError};

use crate::board::Line;

/// The I²C bus shared by the touch controller and the I/O expander.
pub type Bus = Arc<Mutex<I2cDriver<'static>>>;

/// I²C timeout, ticks (ms).
pub const TIMEOUT: u32 = 20;

/// CH422G "write mode" command address.
const CH422G_WR_SET: u8 = 0x24;
/// CH422G "write IO0–IO7 levels" command address.
const CH422G_WR_IO: u8 = 0x38;

/// Drives the board's control lines.
pub struct Lines {
    bus: Bus,
    /// The CH422G's IO0–IO7 output levels, when the board has one.
    expander: Option<Mutex<u8>>,
}

impl Lines {
    /// Take over the control lines. With an expander, switch its IO pins to outputs at
    /// `levels` (bit n = IOn).
    pub fn new(bus: Bus, expander: Option<u8>) -> Result<Self, EspError> {
        if let Some(levels) = expander {
            let mut i2c = bus.lock().map_err(|_| EspError::from_infallible::<{ sys::ESP_FAIL }>())?;
            i2c.write(CH422G_WR_SET, &[0x01], TIMEOUT)?;
            i2c.write(CH422G_WR_IO, &[levels], TIMEOUT)?;
            log::info!("io: CH422G outputs {levels:#010b}");
        }
        Ok(Self { bus, expander: expander.map(Mutex::new) })
    }

    /// Drive `line` high or low.
    pub fn set(&self, line: Line, high: bool) {
        match line {
            Line::Gpio(n) => {
                // Level first, so the pin comes up driving it. `gpio_set_direction` doesn't
                // move a pad off a boot-time function (JTAG, UART…), so select GPIO first.
                // SAFETY: the board reserves this GPIO for this line.
                let r = unsafe {
                    sys::esp!(sys::gpio_set_level(n.into(), high.into())).and_then(|()| {
                        sys::esp_rom_gpio_pad_select_gpio(n.into());
                        sys::esp!(sys::gpio_set_direction(n.into(), sys::gpio_mode_t_GPIO_MODE_OUTPUT))
                    })
                };
                if let Err(e) = r {
                    log::warn!("io: GPIO{n}: {e}");
                }
            }
            Line::Expander(n) => {
                let Some(levels) = &self.expander else {
                    log::warn!("io: EXIO{n} is set but the board has no expander");
                    return;
                };
                let (Ok(mut levels), Ok(mut i2c)) = (levels.lock(), self.bus.lock()) else { return };
                let new = if high { *levels | 1 << n } else { *levels & !(1 << n) };
                if new == *levels {
                    return;
                }
                match i2c.write(CH422G_WR_IO, &[new], TIMEOUT) {
                    Ok(()) => *levels = new,
                    Err(e) => log::warn!("io: CH422G: {e}"),
                }
            }
        }
    }

    /// Hold `line` low for `low_ms`, then high, then wait `settle_ms` (a reset pulse).
    pub fn pulse_low(&self, line: Line, low_ms: u32, settle_ms: u32) {
        self.set(line, false);
        FreeRtos::delay_ms(low_ms);
        self.set(line, true);
        FreeRtos::delay_ms(settle_ms);
    }

    /// Let go of a GPIO (back to a floating input), e.g. the touch INT line after the reset.
    pub fn release(&self, gpio: u8) {
        // SAFETY: plain GPIO reset of a line the board reserves.
        if let Err(e) = sys::esp!(unsafe { sys::gpio_reset_pin(gpio.into()) }) {
            log::warn!("io: GPIO{gpio}: {e}");
        }
    }
}
