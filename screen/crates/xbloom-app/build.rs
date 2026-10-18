//! Embed the admin panel (`screen/admin/`) into the binary, gzip-compressed.
//!
//! Each file becomes an `Asset` with its MIME type and an ETag, so both the Raspberry Pi
//! and the ESP32 serve the same panel straight from flash/RAM with no filesystem access.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs};

fn mime(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "json" | "webmanifest" => "application/json",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(dir).expect("read admin dir").flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(&p, out);
        } else if !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
            out.push(p);
        }
    }
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../admin");
    println!("cargo:rerun-if-changed={}", root.display());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let mut files = Vec::new();
    walk(&root, &mut files);
    let mut code = String::from(
        "/// An embedded admin-panel file (gzip-compressed).\n\
         #[derive(Debug)]\n\
         pub struct Asset {\n    /// URL path.\n    pub path: &'static str,\n    /// MIME type.\n    pub mime: &'static str,\n    /// ETag.\n    pub etag: &'static str,\n    /// Gzipped bytes.\n    pub gz: &'static [u8],\n}\n\n\
         /// The admin panel.\npub static ASSETS: &[Asset] = &[\n",
    );
    for (i, f) in files.iter().enumerate() {
        println!("cargo:rerun-if-changed={}", f.display());
        let rel = f.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        let raw = fs::read(f).unwrap();
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(&raw).unwrap();
        let gz = enc.finish().unwrap();
        let gz_path = out_dir.join(format!("asset-{i}.gz"));
        fs::write(&gz_path, &gz).unwrap();
        let _ = writeln!(
            code,
            "    Asset {{ path: \"/{rel}\", mime: \"{}\", etag: \"\\\"{:016x}\\\"\", gz: include_bytes!({:?}) }},",
            mime(f),
            fnv1a(&raw),
            gz_path.display().to_string()
        );
    }
    code.push_str("];\n");
    fs::write(out_dir.join("assets.rs"), code).unwrap();
}
