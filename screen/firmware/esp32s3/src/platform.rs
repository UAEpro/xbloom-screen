//! Slint on the bare panel: a software-rendered window, the touch input, and the event loop.
//!
//! Slint's MCU mode has no windowing system, so this module is the "platform": it owns the
//! one [`MinimalSoftwareWindow`], tells Slint the time, runs jobs posted from other threads
//! (`slint::invoke_from_event_loop`, used by the app's event bus), feeds touches in, and
//! renders only what changed. A rotated panel (the rotation setting) is drawn turned by the
//! renderer, and touches are turned back to match.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::platform::software_renderer::{MinimalSoftwareWindow, RenderingRotation, RepaintBufferType};
use slint::platform::{EventLoopProxy, Platform, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{EventLoopError, LogicalPosition, PhysicalSize, PlatformError};

use crate::display::Display;
use crate::touch::Gt911;

type Job = Box<dyn FnOnce() + Send>;

/// How often the touch controller is polled (it has no interrupt line on most boards).
const TOUCH_POLL: Duration = Duration::from_millis(16);

/// The ESP32 Slint platform.
pub struct EspPlatform {
    window: Rc<MinimalSoftwareWindow>,
    epoch: Instant,
    tx: Sender<Job>,
    quit: Arc<AtomicBool>,
    hw: RefCell<Option<(Display, Gt911, Receiver<Job>)>>,
    /// Clockwise degrees the picture is turned (0, 90, 180, 270).
    rotation: u16,
}

impl EspPlatform {
    /// Install the platform for `display` + `touch`, with the picture turned clockwise by
    /// `rotation` degrees (call before creating any Slint component).
    pub fn install(display: Display, touch: Gt911, rotation: u16) -> Result<(), PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::SwappedBuffers);
        let (w, h) = display.size();
        // A quarter turn swaps the window's sides.
        window.set_size(if rotation % 180 == 90 { PhysicalSize::new(h, w) } else { PhysicalSize::new(w, h) });
        let (tx, rx) = mpsc::channel();
        slint::platform::set_platform(Box::new(Self {
            window,
            epoch: Instant::now(),
            tx,
            quit: Arc::new(AtomicBool::new(false)),
            hw: RefCell::new(Some((display, touch, rx))),
            rotation,
        }))
        .map_err(|e| PlatformError::Other(format!("{e:?}")))
    }

    /// A touch at panel pixel (x, y) as a window position. The renderer draws window point
    /// (wx, wy) at panel (panel width − wy, wx) for a 90° turn; this is the inverse, then
    /// divided by the scale factor (the interface-size setting).
    fn to_window(&self, (x, y): (u32, u32), (pw, ph): (u32, u32)) -> LogicalPosition {
        let (x, y, pw, ph) = (x as f32, y as f32, pw as f32, ph as f32);
        let (wx, wy) = match self.rotation {
            90 => (y, pw - 1.0 - x),
            180 => (pw - 1.0 - x, ph - 1.0 - y),
            270 => (ph - 1.0 - y, x),
            _ => (x, y),
        };
        let scale = self.window.scale_factor();
        LogicalPosition::new(wx / scale, wy / scale)
    }
}

struct Proxy {
    tx: Sender<Job>,
    quit: Arc<AtomicBool>,
}

impl EventLoopProxy for Proxy {
    fn quit_event_loop(&self) -> Result<(), EventLoopError> {
        self.quit.store(true, Ordering::Release);
        let _ = self.tx.send(Box::new(|| ()));
        Ok(())
    }

    fn invoke_from_event_loop(&self, event: Job) -> Result<(), EventLoopError> {
        self.tx.send(event).map_err(|_| EventLoopError::EventLoopTerminated)
    }
}

impl Platform for EspPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> Duration {
        self.epoch.elapsed()
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
        Some(Box::new(Proxy { tx: self.tx.clone(), quit: Arc::clone(&self.quit) }))
    }

    fn debug_log(&self, arguments: core::fmt::Arguments) {
        log::debug!("slint: {arguments}");
    }

    fn run_event_loop(&self) -> Result<(), PlatformError> {
        let (mut display, mut touch, rx) =
            self.hw.borrow_mut().take().ok_or_else(|| PlatformError::Other("event loop already ran".into()))?;
        let panel = display.size();
        let rotation = match self.rotation {
            90 => RenderingRotation::Rotate90,
            180 => RenderingRotation::Rotate180,
            270 => RenderingRotation::Rotate270,
            _ => RenderingRotation::NoRotation,
        };
        let mut finger: Option<LogicalPosition> = None;
        loop {
            slint::platform::update_timers_and_animations();
            while let Ok(job) = rx.try_recv() {
                job();
            }
            if self.quit.load(Ordering::Acquire) {
                return Ok(());
            }

            // One finger: press, drag, lift.
            match touch.read() {
                Some(point) => {
                    let pos = self.to_window(point, panel);
                    if finger != Some(pos) {
                        self.window.dispatch_event_with_result(WindowEvent::PointerMoved { position: pos }).ok();
                    }
                    if finger.is_none() {
                        let down = WindowEvent::PointerPressed { position: pos, button: PointerEventButton::Left };
                        self.window.dispatch_event_with_result(down).ok();
                    }
                    finger = Some(pos);
                }
                None => {
                    if let Some(pos) = finger.take() {
                        let up = WindowEvent::PointerReleased { position: pos, button: PointerEventButton::Left };
                        self.window.dispatch_event_with_result(up).ok();
                        self.window.dispatch_event_with_result(WindowEvent::PointerExited).ok();
                    }
                }
            }

            self.window.draw_if_needed(|renderer| {
                renderer.set_rendering_rotation(rotation);
                renderer.render(display.back_buffer(), panel.0 as usize);
                display.present();
            });

            // Sleep until the next timer, animation frame, posted job or touch poll.
            let wait = if self.window.has_active_animations() {
                Duration::from_millis(1)
            } else {
                slint::platform::duration_until_next_timer_update().unwrap_or(TOUCH_POLL).min(TOUCH_POLL)
            };
            match rx.recv_timeout(wait.max(Duration::from_millis(1))) {
                Ok(job) => job(),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }
}
