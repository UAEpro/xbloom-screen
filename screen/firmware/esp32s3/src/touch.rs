//! GT911 capacitive touch controller over I²C (polled; the UI needs one finger).
//!
//! Register map (Goodix GT911 programming guide): `0x8140` product id ("911"),
//! `0x8048`/`0x804A` configured X/Y resolution, `0x814E` status (bit 7 = new data, low
//! nibble = points), `0x8150` first point (x, y as little-endian u16). The status register
//! must be cleared after each read.

use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::sys::{self, EspError};

use crate::board::{Line, Touch as TouchPins};
use crate::io::{Bus, Lines, TIMEOUT};

const REG_PRODUCT_ID: u16 = 0x8140;
const REG_X_MAX: u16 = 0x8048;
const REG_STATUS: u16 = 0x814E;
const REG_POINT1: u16 = 0x8150;

/// The touch controller.
pub struct Gt911 {
    i2c: Bus,
    addr: u8,
    /// Controller resolution, to scale into screen pixels.
    native: (u32, u32),
    screen: (u32, u32),
    swap_xy: bool,
    invert_x: bool,
    invert_y: bool,
    last: Option<(u32, u32)>,
}

impl Gt911 {
    /// Reset the controller and probe it on the board's I²C bus.
    pub fn new(i2c: Bus, lines: &Lines, pins: &TouchPins, screen: (u32, u32)) -> Result<Self, EspError> {
        if let Some(rst) = pins.rst {
            // INT low through the reset and for a while after selects address 0x5D (GT911
            // datasheet); boards that don't wire INT get whichever address it powers up with.
            if let Some(int) = pins.int {
                lines.set(Line::Gpio(int), false);
                FreeRtos::delay_ms(10);
            }
            lines.pulse_low(rst, 100, 200);
            if let Some(int) = pins.int {
                lines.release(int);
            }
        }
        let mut me = Self {
            i2c,
            addr: 0x5D,
            native: screen,
            screen,
            swap_xy: pins.swap_xy,
            invert_x: pins.invert_x,
            invert_y: pins.invert_y,
            last: None,
        };
        // The address depends on the INT level during reset, which not every board drives.
        let mut id = [0u8; 4];
        let found = [0x5D, 0x14].into_iter().find(|&a| {
            me.addr = a;
            me.read_reg(REG_PRODUCT_ID, &mut id).is_ok()
        });
        match found {
            Some(a) => log::info!("touch: GT{} at 0x{a:02X}", String::from_utf8_lossy(&id[..3])),
            None => log::warn!("touch: no GT911 answered on 0x5D/0x14"),
        }
        let mut res = [0u8; 4];
        if me.read_reg(REG_X_MAX, &mut res).is_ok() {
            let (x, y) =
                (u32::from(u16::from_le_bytes([res[0], res[1]])), u32::from(u16::from_le_bytes([res[2], res[3]])));
            if x > 0 && y > 0 {
                me.native = (x, y);
            }
        }
        log::info!("touch: {}×{} → {}×{}", me.native.0, me.native.1, screen.0, screen.1);
        Ok(me)
    }

    fn read_reg(&mut self, reg: u16, buf: &mut [u8]) -> Result<(), EspError> {
        let mut i2c = self.i2c.lock().map_err(|_| EspError::from_infallible::<{ sys::ESP_FAIL }>())?;
        i2c.write_read(self.addr, &reg.to_be_bytes(), buf, TIMEOUT)
    }

    fn write_reg(&mut self, reg: u16, value: u8) -> Result<(), EspError> {
        let [hi, lo] = reg.to_be_bytes();
        let mut i2c = self.i2c.lock().map_err(|_| EspError::from_infallible::<{ sys::ESP_FAIL }>())?;
        i2c.write(self.addr, &[hi, lo, value], TIMEOUT)
    }

    /// The finger position in screen pixels, or `None` when nothing touches the glass.
    pub fn read(&mut self) -> Option<(u32, u32)> {
        let mut status = [0u8; 1];
        if self.read_reg(REG_STATUS, &mut status).is_err() {
            return None;
        }
        if status[0] & 0x80 == 0 {
            // No new report: the finger (if any) hasn't moved.
            return self.last;
        }
        let points = status[0] & 0x0F;
        let mut p = [0u8; 8];
        let pos = if points > 0 && self.read_reg(REG_POINT1, &mut p).is_ok() {
            let x = u32::from(u16::from_le_bytes([p[1], p[2]]));
            let y = u32::from(u16::from_le_bytes([p[3], p[4]]));
            Some(self.map(x, y))
        } else {
            None
        };
        let _ = self.write_reg(REG_STATUS, 0);
        self.last = pos;
        pos
    }

    fn map(&self, x: u32, y: u32) -> (u32, u32) {
        let (mut x, mut y) = if self.swap_xy { (y, x) } else { (x, y) };
        let (nw, nh) = if self.swap_xy { (self.native.1, self.native.0) } else { self.native };
        let (w, h) = self.screen;
        x = (x * w / nw.max(1)).min(w - 1);
        y = (y * h / nh.max(1)).min(h - 1);
        if self.invert_x {
            x = w - 1 - x;
        }
        if self.invert_y {
            y = h - 1 - y;
        }
        (x, y)
    }
}
