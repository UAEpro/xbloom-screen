//! Slint on the bare panel: a software-rendered window, the touch input, and the event loop.
//!
//! Slint's MCU mode has no windowing system, so this module is the "platform": it owns the
//! one [`MinimalSoftwareWindow`], tells Slint the time, runs jobs posted from other threads
//! (`slint::invoke_from_event_loop`, used by the app's event bus), feeds touches in, and
//! renders only what changed.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
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
}

impl EspPlatform {
    /// Install the platform for `display` + `touch` (call before creating any Slint component).
    pub fn install(display: Display, touch: Gt911) -> Result<(), PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::SwappedBuffers);
        let (w, h) = display.size();
        window.set_size(PhysicalSize::new(w, h));
        let (tx, rx) = mpsc::channel();
        slint::platform::set_platform(Box::new(Self {
            window,
            epoch: Instant::now(),
            tx,
            quit: Arc::new(AtomicBool::new(false)),
            hw: RefCell::new(Some((display, touch, rx))),
        }))
        .map_err(|e| PlatformError::Other(format!("{e:?}")))
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
        let (width, _) = display.size();
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
                Some((x, y)) => {
                    let pos = LogicalPosition::new(x as f32, y as f32);
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
                renderer.render(display.back_buffer(), width as usize);
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
