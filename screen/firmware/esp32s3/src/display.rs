//! The RGB panel (ESP-IDF `esp_lcd`) and its PWM backlight.
//!
//! Two full-screen RGB565 frame buffers live in PSRAM. Slint renders the changed parts of a
//! frame into the back buffer, then [`Display::present`] hands it to the LCD and waits until
//! the LCD has stopped reading the old one, so the screen never shows a half-drawn frame. The
//! LCD peripheral reads the front buffer through two small bounce buffers in internal RAM,
//! which keeps the picture stable while the CPU and Wi-Fi also hammer PSRAM.

use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use esp_idf_svc::hal::gpio::AnyOutputPin;
use esp_idf_svc::hal::ledc::config::TimerConfig;
use esp_idf_svc::hal::ledc::{LedcChannel, LedcDriver, LedcTimer, LedcTimerDriver, LowSpeed, Resolution};
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::sys::{self, esp, EspError};
use slint::platform::software_renderer::Rgb565Pixel;

use crate::board::Board;

/// Bounce buffer height, lines (two of them, in internal RAM).
const BOUNCE_LINES: usize = 10;

/// Set by the UI task after queueing a new frame buffer; the interrupt clears it and wakes
/// the task.
static WAITING: AtomicBool = AtomicBool::new(false);
/// The FreeRTOS task waiting in [`Display::present`].
static WAITER: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// Bounce-buffer interrupt, once per frame: the driver has just switched to the newest frame
/// buffer and finished reading the previous one, which is now free to draw into.
unsafe extern "C" fn on_frame_buf_complete(
    _panel: sys::esp_lcd_panel_handle_t,
    _edata: *const sys::esp_lcd_rgb_panel_event_data_t,
    _ctx: *mut c_void,
) -> bool {
    if !WAITING.swap(false, Ordering::AcqRel) {
        return false;
    }
    let task = WAITER.load(Ordering::Acquire);
    if task.is_null() {
        return false;
    }
    let mut woken: sys::BaseType_t = 0;
    // SAFETY: `task` is the live UI task handle; FromISR variant in interrupt context.
    unsafe {
        sys::xTaskGenericNotifyFromISR(task.cast(), 0, 1, sys::eNotifyAction_eSetBits, ptr::null_mut(), &mut woken)
    };
    // Returning true asks the driver to yield to the woken task on ISR exit.
    woken != 0
}

/// The panel with its two frame buffers.
pub struct Display {
    panel: sys::esp_lcd_panel_handle_t,
    fbs: [*mut Rgb565Pixel; 2],
    back: usize,
    width: u32,
    height: u32,
}

impl Display {
    /// Bring up the panel described by `board`.
    pub fn new(board: &Board) -> Result<Self, EspError> {
        let p = &board.panel;
        // SAFETY: plain C config struct; every field the driver reads is set below.
        let mut cfg: sys::esp_lcd_rgb_panel_config_t = unsafe { core::mem::zeroed() };
        cfg.clk_src = sys::soc_periph_lcd_clk_src_t_LCD_CLK_SRC_DEFAULT;
        cfg.timings.pclk_hz = p.pclk_hz;
        cfg.timings.h_res = board.width;
        cfg.timings.v_res = board.height;
        (cfg.timings.hsync_pulse_width, cfg.timings.hsync_back_porch, cfg.timings.hsync_front_porch) = p.hsync;
        (cfg.timings.vsync_pulse_width, cfg.timings.vsync_back_porch, cfg.timings.vsync_front_porch) = p.vsync;
        cfg.timings.flags.set_pclk_active_neg(u32::from(p.pclk_active_neg));
        cfg.data_width = 16;
        cfg.bits_per_pixel = 16;
        cfg.num_fbs = 2;
        cfg.bounce_buffer_size_px = board.width as usize * BOUNCE_LINES;
        cfg.__bindgen_anon_1.dma_burst_size = 64;
        cfg.hsync_gpio_num = p.hsync_gpio;
        cfg.vsync_gpio_num = p.vsync_gpio;
        cfg.de_gpio_num = p.de_gpio;
        cfg.pclk_gpio_num = p.pclk_gpio;
        cfg.disp_gpio_num = -1;
        cfg.data_gpio_nums = p.data;
        cfg.flags.set_fb_in_psram(1);

        let mut panel: sys::esp_lcd_panel_handle_t = ptr::null_mut();
        // SAFETY: valid config; the driver writes the handle.
        unsafe {
            esp!(sys::esp_lcd_new_rgb_panel(&cfg, &mut panel))?;
            let mut cbs: sys::esp_lcd_rgb_panel_event_callbacks_t = core::mem::zeroed();
            cbs.__bindgen_anon_1.on_frame_buf_complete = Some(on_frame_buf_complete);
            esp!(sys::esp_lcd_rgb_panel_register_event_callbacks(panel, &cbs, ptr::null_mut()))?;
            esp!(sys::esp_lcd_panel_reset(panel))?;
            esp!(sys::esp_lcd_panel_init(panel))?;
        }
        let mut fb0: *mut c_void = ptr::null_mut();
        let mut fb1: *mut c_void = ptr::null_mut();
        // SAFETY: the panel owns two frame buffers; the call writes both pointers.
        esp!(unsafe { sys::esp_lcd_rgb_panel_get_frame_buffer(panel, 2, &mut fb0 as *mut _, &mut fb1 as *mut _) })?;
        log::info!("display: {}×{} RGB565, frame buffers in PSRAM", board.width, board.height);
        // The driver shows fb0 first, so Slint draws into fb1.
        Ok(Self { panel, fbs: [fb0.cast(), fb1.cast()], back: 1, width: board.width, height: board.height })
    }

    /// Panel size, px.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The buffer to draw the next frame into (not on screen).
    pub fn back_buffer(&mut self) -> &mut [Rgb565Pixel] {
        let len = (self.width * self.height) as usize;
        // SAFETY: each frame buffer is width × height RGB565 pixels, owned by the panel for
        // its whole life, and the back one isn't read by the LCD until `present`.
        unsafe { core::slice::from_raw_parts_mut(self.fbs[self.back], len) }
    }

    /// Show the back buffer; returns once the old front buffer is free to draw into.
    pub fn present(&mut self) {
        // SAFETY: plain FreeRTOS / esp_lcd calls on the current task.
        unsafe {
            WAITER.store(sys::xTaskGetCurrentTaskHandle().cast(), Ordering::Release);
            // Drop a stale wake-up.
            sys::ulTaskGenericNotifyTake(0, 1, 0);
            // Passing one of the panel's own frame buffers makes the driver switch to it at
            // the start of the next frame instead of copying it.
            let buf = self.fbs[self.back];
            sys::esp_lcd_panel_draw_bitmap(self.panel, 0, 0, self.width as i32, self.height as i32, buf.cast());
            // Armed only after the switch is queued, so the wake-up can't be for an older one.
            // If a frame boundary slips in between, the next one wakes us (one frame later).
            WAITING.store(true, Ordering::Release);
            if sys::ulTaskGenericNotifyTake(0, 1, 100) == 0 {
                WAITING.store(false, Ordering::Release);
            }
        }
        self.back ^= 1;
    }
}

/// The backlight, dimmable by PWM.
pub struct Backlight {
    pwm: LedcDriver<'static>,
}

impl Backlight {
    /// PWM on `gpio`, starting dark (the first frame turns it on).
    pub fn new(
        timer: impl LedcTimer<SpeedMode = LowSpeed> + 'static,
        channel: impl LedcChannel<SpeedMode = LowSpeed> + 'static,
        gpio: u8,
    ) -> Result<Self, EspError> {
        let timer =
            LedcTimerDriver::new(timer, &TimerConfig::new().frequency(Hertz(20_000)).resolution(Resolution::Bits10))?;
        // SAFETY: the board reserves this GPIO for the backlight; nothing else claims it.
        let pin = unsafe { AnyOutputPin::steal(gpio) };
        let mut pwm = LedcDriver::new(channel, timer, pin)?;
        pwm.set_duty(0)?;
        Ok(Self { pwm })
    }

    /// Brightness 0–100 %, on a perceptual (squared) curve; 0 turns the light off.
    pub fn set(&mut self, percent: u8) {
        let max = self.pwm.get_max_duty();
        let p = u32::from(percent.min(100));
        let duty = if p == 0 { 0 } else { (max * p * p / 10_000).max(max / 100).max(1) };
        if let Err(e) = self.pwm.set_duty(duty) {
            log::warn!("backlight: {e}");
        }
    }
}
