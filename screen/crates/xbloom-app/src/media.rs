//! Screensaver media: looping videos (`*.xbv`) and photos (`*.jpg`).
//!
//! ## The `XBV1` video container
//!
//! Videos are stored as a sequence of JPEG frames — Motion-JPEG — so the *same* tiny decoder
//! plays them on a Raspberry Pi and on an ESP32-S3 (which has no video decoder). The admin
//! panel converts any video the browser can play (MP4, WebM, MOV…) into this format **in the
//! browser**, at the resolution and frame rate the device asks for, so the device never needs
//! ffmpeg. All integers are little-endian:
//!
//! ```text
//! 0   "XBV1"               magic
//! 4   u16 width            frame size in pixels
//! 6   u16 height
//! 8   u16 fps × 100        e.g. 2400 = 24 fps
//! 10  u16 flags            bit 0 = loop (always set today)
//! 12  u32 frame_count
//! 16  u32 reserved (0)
//! 20  frame_count × { u32 offset, u32 length }   from the start of the file
//! …   JPEG frames (each starts with FF D8)
//! ```

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use crate::util::{random_hex, slug};

/// The container magic.
pub const XBV_MAGIC: &[u8; 4] = b"XBV1";
const HEADER_LEN: u64 = 20;
/// Most frames accepted in one video.
pub const MAX_FRAMES: u32 = 20_000;

/// A parsed `XBV1` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct XbvHeader {
    /// Frame width, px.
    pub width: u16,
    /// Frame height, px.
    pub height: u16,
    /// Frames per second × 100.
    pub fps_x100: u16,
    /// Number of frames.
    pub frames: u32,
}

impl XbvHeader {
    /// Frames per second.
    pub fn fps(&self) -> f32 {
        f32::from(self.fps_x100.max(1)) / 100.0
    }

    /// Duration, seconds.
    pub fn duration_s(&self) -> f32 {
        self.frames as f32 / self.fps()
    }
}

/// Reads frames from an `XBV1` file on demand (only the index is held in memory).
#[derive(Debug)]
pub struct XbvReader {
    file: File,
    /// The header.
    pub header: XbvHeader,
    index: Vec<(u32, u32)>,
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

impl XbvReader {
    /// Open and validate an `XBV1` file.
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let mut h = [0u8; HEADER_LEN as usize];
        file.read_exact(&mut h).map_err(|_| bad("file too short for an XBV1 header"))?;
        if &h[0..4] != XBV_MAGIC {
            return Err(bad("not an XBV1 video (convert it in the admin panel)"));
        }
        let u16_at = |i: usize| u16::from_le_bytes([h[i], h[i + 1]]);
        let header = XbvHeader {
            width: u16_at(4),
            height: u16_at(6),
            fps_x100: u16_at(8),
            frames: u32::from_le_bytes([h[12], h[13], h[14], h[15]]),
        };
        if !(16..=4096).contains(&header.width) || !(16..=4096).contains(&header.height) {
            return Err(bad("unsupported frame size"));
        }
        if !(100..=6000).contains(&header.fps_x100) {
            return Err(bad("frame rate must be 1–60 fps"));
        }
        if header.frames == 0 || header.frames > MAX_FRAMES {
            return Err(bad("frame count out of range"));
        }
        let mut raw = vec![0u8; header.frames as usize * 8];
        file.read_exact(&mut raw).map_err(|_| bad("truncated frame index"))?;
        let data_start = HEADER_LEN + raw.len() as u64;
        let index: Vec<(u32, u32)> = raw
            .chunks_exact(8)
            .map(|c| (u32::from_le_bytes([c[0], c[1], c[2], c[3]]), u32::from_le_bytes([c[4], c[5], c[6], c[7]])))
            .collect();
        for &(off, n) in &index {
            if u64::from(off) < data_start || u64::from(off) + u64::from(n) > len || n < 4 {
                return Err(bad("frame index points outside the file"));
            }
        }
        let mut r = Self { file, header, index };
        let first = r.frame(0)?;
        if first[..2] != [0xFF, 0xD8] {
            return Err(bad("frames are not JPEG"));
        }
        Ok(r)
    }

    /// Number of frames.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// True if there are no frames (never, for a validated file).
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Read frame `i`'s JPEG bytes.
    pub fn frame(&mut self, i: usize) -> io::Result<Vec<u8>> {
        let (off, n) = *self.index.get(i).ok_or_else(|| bad("frame out of range"))?;
        self.file.seek(SeekFrom::Start(u64::from(off)))?;
        let mut buf = vec![0u8; n as usize];
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }
}

/// Build an `XBV1` file from JPEG frames (tests and tooling; the admin panel has its own).
pub fn write_xbv(out: &mut impl Write, width: u16, height: u16, fps_x100: u16, frames: &[Vec<u8>]) -> io::Result<()> {
    let n = frames.len() as u32;
    let mut header = Vec::with_capacity(HEADER_LEN as usize + frames.len() * 8);
    header.extend_from_slice(XBV_MAGIC);
    header.extend_from_slice(&width.to_le_bytes());
    header.extend_from_slice(&height.to_le_bytes());
    header.extend_from_slice(&fps_x100.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&n.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    let mut off = HEADER_LEN as u32 + n * 8;
    for f in frames {
        header.extend_from_slice(&off.to_le_bytes());
        header.extend_from_slice(&(f.len() as u32).to_le_bytes());
        off += f.len() as u32;
    }
    out.write_all(&header)?;
    for f in frames {
        out.write_all(f)?;
    }
    Ok(())
}

/// A decoded RGB8 image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbImage {
    /// Width, px.
    pub width: u32,
    /// Height, px.
    pub height: u32,
    /// Tightly packed RGB bytes.
    pub rgb: Vec<u8>,
}

/// Decode a JPEG to RGB8 (pure Rust, SIMD where the CPU has it).
pub fn decode_jpeg(bytes: &[u8]) -> Result<RgbImage, String> {
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut d = JpegDecoder::new_with_options(bytes, opts);
    let rgb = d.decode().map_err(|e| format!("{e:?}"))?;
    let info = d.info().ok_or("no image info")?;
    Ok(RgbImage { width: u32::from(info.width), height: u32::from(info.height), rgb })
}

/// Read a JPEG's dimensions without decoding it.
pub fn jpeg_dimensions(bytes: &[u8]) -> Option<(u16, u16)> {
    let mut d = JpegDecoder::new(bytes);
    d.decode_headers().ok()?;
    d.info().map(|i| (i.width, i.height))
}

/// What kind of media a file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    /// An `XBV1` video.
    Video,
    /// A JPEG photo.
    Image,
}

/// A media file, for listings.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MediaInfo {
    /// File name (`sunrise.xbv`).
    pub name: String,
    /// Video or image.
    pub kind: MediaKind,
    /// Bytes on disk.
    pub size: u64,
    /// Width, px.
    pub width: u16,
    /// Height, px.
    pub height: u16,
    /// Frames (1 for an image).
    pub frames: u32,
    /// Frames per second (0 for an image).
    pub fps: f32,
}

/// The media directory.
#[derive(Debug, Clone)]
pub struct MediaStore {
    dir: PathBuf,
    /// Largest accepted upload, bytes.
    pub max_upload: u64,
}

/// An upload in progress: bytes go to a temp file, validated on [`Upload::finish`].
#[derive(Debug)]
pub struct Upload {
    tmp: PathBuf,
    dest: PathBuf,
    file: File,
    written: u64,
    max: u64,
}

impl Upload {
    /// Append a chunk.
    pub fn write(&mut self, chunk: &[u8]) -> io::Result<()> {
        self.written += chunk.len() as u64;
        if self.written > self.max {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!("upload exceeds {} MB", self.max / 1_000_000),
            ));
        }
        self.file.write_all(chunk)
    }

    /// Stream everything from `r` into the upload.
    pub fn copy_from(&mut self, r: &mut dyn Read) -> io::Result<u64> {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                return Ok(self.written);
            }
            self.write(&buf[..n])?;
        }
    }

    /// Validate and move into place. On failure the temp file is removed.
    pub fn finish(mut self) -> io::Result<MediaInfo> {
        self.file.flush()?;
        self.file.sync_all()?;
        let info = MediaStore::inspect(&self.tmp).and_then(|mut i| {
            fs::rename(&self.tmp, &self.dest)?;
            i.name = file_name(&self.dest);
            Ok(i)
        });
        if info.is_err() {
            let _ = fs::remove_file(&self.tmp);
        }
        info
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        // No-op after a successful rename; cleans up an abandoned upload.
        let _ = fs::remove_file(&self.tmp);
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

impl MediaStore {
    /// Media in `dir` (created if missing).
    pub fn new(dir: PathBuf, max_upload: u64) -> Self {
        let _ = fs::create_dir_all(&dir);
        Self { dir, max_upload }
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Normalise an uploaded name to a safe `slug.ext`, or `None` for an unsupported type.
    pub fn safe_name(name: &str) -> Option<String> {
        let (stem, ext) = name.rsplit_once('.')?;
        let ext = match ext.to_ascii_lowercase().as_str() {
            "xbv" => "xbv",
            "jpg" | "jpeg" => "jpg",
            _ => return None,
        };
        let mut s = slug(stem);
        s.truncate(48);
        Some(format!("{s}.{ext}"))
    }

    /// Path of a stored file, if the name is safe and it exists.
    pub fn path(&self, name: &str) -> Option<PathBuf> {
        let safe = Self::safe_name(name)?;
        (safe == name).then(|| self.dir.join(&safe)).filter(|p| p.is_file())
    }

    fn inspect(path: &Path) -> io::Result<MediaInfo> {
        let size = fs::metadata(path)?.len();
        let name = file_name(path);
        let mut magic = [0u8; 4];
        File::open(path)?.read_exact(&mut magic).map_err(|_| bad("empty file"))?;
        if &magic == XBV_MAGIC {
            let r = XbvReader::open(path)?;
            let h = r.header;
            return Ok(MediaInfo {
                name,
                kind: MediaKind::Video,
                size,
                width: h.width,
                height: h.height,
                frames: h.frames,
                fps: h.fps(),
            });
        }
        if magic[..2] == [0xFF, 0xD8] {
            let bytes = fs::read(path)?;
            let (w, h) = jpeg_dimensions(&bytes).ok_or_else(|| bad("unreadable JPEG"))?;
            return Ok(MediaInfo { name, kind: MediaKind::Image, size, width: w, height: h, frames: 1, fps: 0.0 });
        }
        Err(bad("unsupported file: upload a converted video (.xbv) or a JPEG photo"))
    }

    /// All media, videos first then photos, by name.
    pub fn list(&self) -> Vec<MediaInfo> {
        let mut out: Vec<MediaInfo> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "xbv" || e == "jpg"))
            .filter_map(|p| Self::inspect(&p).ok())
            .collect();
        out.sort_by(|a, b| (a.kind == MediaKind::Image, &a.name).cmp(&(b.kind == MediaKind::Image, &b.name)));
        out
    }

    /// Photos for the slideshow.
    pub fn images(&self) -> Vec<PathBuf> {
        self.list().into_iter().filter(|m| m.kind == MediaKind::Image).map(|m| self.dir.join(m.name)).collect()
    }

    /// Start an upload named `name` (normalised; an existing file is replaced on finish).
    pub fn begin_upload(&self, name: &str) -> io::Result<Upload> {
        let safe = Self::safe_name(name).ok_or_else(|| bad("only .xbv videos and .jpg photos are accepted"))?;
        let tmp = self.dir.join(format!(".upload-{}", random_hex(4)));
        let file = File::create(&tmp)?;
        Ok(Upload { tmp, dest: self.dir.join(safe), file, written: 0, max: self.max_upload })
    }

    /// Delete a file. Returns whether it existed.
    pub fn delete(&self, name: &str) -> bool {
        self.path(name).is_some_and(|p| fs::remove_file(p).is_ok())
    }

    /// A JPEG preview: the first frame of a video, or the photo itself.
    pub fn thumbnail(&self, name: &str) -> io::Result<Vec<u8>> {
        let p = self.path(name).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such media"))?;
        if p.extension().is_some_and(|e| e == "xbv") {
            XbvReader::open(&p)?.frame(0)
        } else {
            fs::read(p)
        }
    }

    /// Remove abandoned partial uploads (call at boot).
    pub fn cleanup(&self) {
        for e in fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().starts_with(".upload-") {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A real 16×16 baseline JPEG, encoded at test time.
    pub fn tiny_jpeg() -> Vec<u8> {
        let mut out = Vec::new();
        let rgb: Vec<u8> = (0..16 * 16).flat_map(|i| [i as u8, 128, 255 - i as u8]).collect();
        jpeg_encoder::Encoder::new(&mut out, 85).encode(&rgb, 16, 16, jpeg_encoder::ColorType::Rgb).unwrap();
        out
    }

    #[test]
    fn tiny_jpeg_decodes() {
        let jpg = tiny_jpeg();
        let img = decode_jpeg(&jpg).unwrap();
        assert_eq!((img.width, img.height, img.rgb.len()), (16, 16, 16 * 16 * 3));
        assert_eq!(jpeg_dimensions(&jpg), Some((16, 16)));
    }

    #[test]
    fn xbv_round_trip_and_upload_validation() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path().join("media"), 1_000_000);
        let mut buf = Vec::new();
        let jpg = tiny_jpeg();
        write_xbv(&mut buf, 16, 16, 2400, &[jpg.clone(), jpg.clone()]).unwrap();
        let mut up = store.begin_upload("My Clip!.XBV").unwrap();
        up.copy_from(&mut &buf[..]).unwrap();
        let info = up.finish().unwrap();
        assert_eq!((info.name.as_str(), info.kind, info.frames), ("my-clip.xbv", MediaKind::Video, 2));
        assert_eq!(store.thumbnail("my-clip.xbv").unwrap(), jpg);
        let mut r = XbvReader::open(&store.path("my-clip.xbv").unwrap()).unwrap();
        assert_eq!(r.frame(1).unwrap(), jpg);
        // Garbage is rejected and leaves nothing behind.
        let mut bad = store.begin_upload("junk.xbv").unwrap();
        bad.write(b"XBV1nonsense").unwrap();
        assert!(bad.finish().is_err());
        assert_eq!(store.list().len(), 1);
        // Path traversal can't escape the directory.
        assert!(store.path("../settings.json").is_none());
        assert!(MediaStore::safe_name("x.exe").is_none());
    }

    #[test]
    fn upload_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path().to_path_buf(), 10);
        let mut up = store.begin_upload("a.jpg").unwrap();
        assert!(up.write(&[0u8; 11]).is_err());
    }
}
