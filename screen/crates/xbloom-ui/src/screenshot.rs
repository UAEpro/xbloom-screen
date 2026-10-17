//! Offscreen rendering of the UI to PNG files (docs and visual checks).
//!
//! Installs a minimal Slint platform backed by a [`MinimalSoftwareWindow`] — the same kind of
//! integration the ESP32 firmware uses — runs timers and posted callbacks by hand, and saves
//! frames with the software renderer.

use std::collections::VecDeque;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{EventLoopProxy, Platform, WindowAdapter};
use slint::{EventLoopError, PhysicalSize, PlatformError, Rgb8Pixel};

type Job = Box<dyn FnOnce() + Send>;

struct Proxy(Arc<Mutex<VecDeque<Job>>>);

impl EventLoopProxy for Proxy {
    fn quit_event_loop(&self) -> Result<(), EventLoopError> {
        Ok(())
    }
    fn invoke_from_event_loop(&self, event: Job) -> Result<(), EventLoopError> {
        self.0.lock().unwrap().push_back(event);
        Ok(())
    }
}

struct ShotPlatform {
    window: Rc<MinimalSoftwareWindow>,
    start: Instant,
    queue: Arc<Mutex<VecDeque<Job>>>,
}

impl Platform for ShotPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }
    fn duration_since_start(&self) -> Duration {
        self.start.elapsed()
    }
    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
        Some(Box::new(Proxy(Arc::clone(&self.queue))))
    }
}

/// An offscreen window that renders to PNG.
pub struct Shooter {
    window: Rc<MinimalSoftwareWindow>,
    queue: Arc<Mutex<VecDeque<Job>>>,
    width: u32,
    height: u32,
}

impl Shooter {
    /// Install the offscreen platform (call before creating the `AppWindow`).
    pub fn install(width: u32, height: u32) -> Self {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        window.set_size(PhysicalSize::new(width, height));
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        slint::platform::set_platform(Box::new(ShotPlatform {
            window: window.clone(),
            start: Instant::now(),
            queue: Arc::clone(&queue),
        }))
        .expect("platform already set");
        Self { window, queue, width, height }
    }

    /// Run timers, animations and posted callbacks for `d`.
    pub fn pump(&self, d: Duration) {
        let until = Instant::now() + d;
        loop {
            slint::platform::update_timers_and_animations();
            loop {
                let job = self.queue.lock().unwrap().pop_front();
                match job {
                    Some(j) => j(),
                    None => break,
                }
            }
            if Instant::now() >= until {
                break;
            }
            std::thread::sleep(Duration::from_millis(8));
        }
    }

    /// Render the current frame and save it as a PNG.
    pub fn capture(&self, path: &Path) -> std::io::Result<()> {
        self.pump(Duration::from_millis(20));
        let mut buf = vec![Rgb8Pixel { r: 0, g: 0, b: 0 }; (self.width * self.height) as usize];
        self.window.request_redraw();
        self.window.draw_if_needed(|renderer| {
            renderer.render(&mut buf, self.width as usize);
        });
        let file = std::fs::File::create(path)?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), self.width, self.height);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().map_err(std::io::Error::other)?;
        let bytes: Vec<u8> = buf.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
        w.write_image_data(&bytes).map_err(std::io::Error::other)?;
        Ok(())
    }
}
