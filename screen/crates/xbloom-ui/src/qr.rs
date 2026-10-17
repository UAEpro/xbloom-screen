//! A QR code for the admin-panel URL, as a Slint image.

use qrcodegen::{QrCode, QrCodeEcc};
use slint::{Image, Rgb8Pixel, SharedPixelBuffer};

/// Render `text` as a crisp QR code (`scale` pixels per module, with a 2-module quiet zone).
pub fn qr_pixels(text: &str, scale: u32) -> Option<SharedPixelBuffer<Rgb8Pixel>> {
    let qr = QrCode::encode_text(text, QrCodeEcc::Medium).ok()?;
    let quiet = 2;
    let modules = qr.size() as u32 + 2 * quiet;
    let px = modules * scale;
    let mut buf = SharedPixelBuffer::<Rgb8Pixel>::new(px, px);
    let ink = Rgb8Pixel { r: 0x1f, g: 0x17, b: 0x12 };
    let paper = Rgb8Pixel { r: 0xff, g: 0xff, b: 0xff };
    let data = buf.make_mut_slice();
    for y in 0..px {
        for x in 0..px {
            let mx = (x / scale) as i32 - quiet as i32;
            let my = (y / scale) as i32 - quiet as i32;
            data[(y * px + x) as usize] = if qr.get_module(mx, my) { ink } else { paper };
        }
    }
    Some(buf)
}

/// The QR code as an image (must be called on the UI thread).
pub fn qr_image(text: &str) -> Image {
    qr_pixels(text, 6).map(Image::from_rgb8).unwrap_or_default()
}

/// The standard "join this Wi-Fi" QR payload (`WIFI:T:WPA;S:…;P:…;;`) that phone cameras
/// understand.
pub fn wifi_join(ssid: &str, password: &str) -> String {
    fn esc(s: &str) -> String {
        s.chars().flat_map(|c| if "\\;,:\"".contains(c) { vec!['\\', c] } else { vec![c] }).collect()
    }
    format!("WIFI:T:WPA;S:{};P:{};;", esc(ssid), esc(password))
}

#[cfg(test)]
mod tests {
    #[test]
    fn wifi_payload_escapes_specials() {
        assert_eq!(super::wifi_join("xBloom-1A2B", "pa;ss"), "WIFI:T:WPA;S:xBloom-1A2B;P:pa\\;ss;;");
    }
}
