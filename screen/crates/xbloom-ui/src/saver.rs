//! The screensaver's frame source: plays an `XBV1` video or cycles photos on a worker thread.
//!
//! Frames are decoded off the UI thread and handed over one at a time: while a frame is still
//! waiting to be shown, the player doesn't decode the next one, and the frame index follows the
//! wall clock — so a slow board (an ESP32) drops frames but keeps the clip's real speed instead
//! of building a backlog.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use slint::{Rgb8Pixel, SharedPixelBuffer};
use xbloom_app::media::{decode_jpeg, XbvReader};

/// What to play.
#[derive(Debug, Clone, PartialEq)]
pub enum SaverContent {
    /// Loop a video.
    Video(PathBuf),
    /// Cycle photos, one every `interval`.
    Slideshow(Vec<PathBuf>, Duration),
}

/// Receives decoded frames. Call [`FrameGate::done`] once the frame is on screen.
pub type FrameSink = Box<dyn Fn(SharedPixelBuffer<Rgb8Pixel>, FrameGate) + Send>;

/// Back-pressure token: the player waits for it before decoding the next frame.
#[derive(Clone)]
pub struct FrameGate(Arc<AtomicBool>);

impl FrameGate {
    /// The frame was displayed; the player may deliver the next one.
    pub fn done(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// A running player. Dropping it stops the thread.
pub struct SaverPlayer {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

fn to_buffer(jpeg: &[u8]) -> Option<SharedPixelBuffer<Rgb8Pixel>> {
    match decode_jpeg(jpeg) {
        Ok(img) => Some(SharedPixelBuffer::clone_from_slice(&img.rgb, img.width, img.height)),
        Err(e) => {
            log::warn!("screensaver: undecodable frame: {e}");
            None
        }
    }
}

impl SaverPlayer {
    /// Start playing `content` into `sink`.
    pub fn start(content: SaverContent, sink: FrameSink) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("xbloom-saver".into())
            .stack_size(256 * 1024) // JPEG decoding is stack-hungry
            .spawn(move || run(content, &sink, &stop2))
            .ok();
        Self { stop, handle }
    }
}

impl Drop for SaverPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn sleep_unless_stopped(d: Duration, stop: &AtomicBool) {
    let until = Instant::now() + d;
    while !stop.load(Ordering::Acquire) {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        thread::sleep(left.min(Duration::from_millis(50)));
    }
}

fn run(content: SaverContent, sink: &FrameSink, stop: &AtomicBool) {
    let busy = Arc::new(AtomicBool::new(false));
    let deliver = |buf| {
        busy.store(true, Ordering::Release);
        sink(buf, FrameGate(Arc::clone(&busy)));
    };
    match content {
        SaverContent::Video(path) => {
            let mut reader = match XbvReader::open(&path) {
                Ok(r) => r,
                Err(e) => return log::warn!("screensaver: can't play {}: {e}", path.display()),
            };
            let fps = reader.header.fps();
            let n = reader.len();
            let period = Duration::from_secs_f32(1.0 / fps);
            let t0 = Instant::now();
            let mut last = usize::MAX;
            while !stop.load(Ordering::Acquire) {
                if busy.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(4));
                    continue;
                }
                let idx = ((t0.elapsed().as_secs_f32() * fps) as usize) % n;
                if idx != last {
                    last = idx;
                    if let Some(buf) = reader.frame(idx).ok().and_then(|j| to_buffer(&j)) {
                        deliver(buf);
                    }
                }
                sleep_unless_stopped(period / 2, stop);
            }
        }
        SaverContent::Slideshow(paths, interval) => {
            if paths.is_empty() {
                return;
            }
            let mut i = 0;
            while !stop.load(Ordering::Acquire) {
                if let Some(buf) = std::fs::read(&paths[i % paths.len()]).ok().and_then(|j| to_buffer(&j)) {
                    deliver(buf);
                }
                i += 1;
                sleep_unless_stopped(interval, stop);
            }
        }
    }
}
