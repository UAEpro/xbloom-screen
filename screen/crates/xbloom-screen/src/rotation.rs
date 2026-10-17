//! Screen rotation on a kiosk Pi.
//!
//! Slint's KMS backend turns the picture (`SLINT_KMS_ROTATION`) but passes touches through in
//! the panel's own orientation, so when a touchscreen appears its libinput calibration matrix
//! is turned to match. Any matrix already set through udev (`LIBINPUT_CALIBRATION_MATRIX`) is
//! kept and the rotation is applied on top of it.

/// libinput calibration matrix (row-major 2 × 3, on 0–1 device coordinates) that maps a touch
/// on the panel to the window when Slint draws the window turned clockwise by `degrees`.
///
/// Slint's 90° turn draws window point (x, y) at panel pixel (panel width − y, x), so a touch
/// at panel (u, v) is window (v, 1 − u), in fractions of each side.
pub fn touch_matrix(degrees: u16) -> [f32; 6] {
    match degrees {
        90 => [0.0, 1.0, 0.0, -1.0, 0.0, 1.0],
        180 => [-1.0, 0.0, 1.0, 0.0, -1.0, 1.0],
        270 => [0.0, -1.0, 1.0, 1.0, 0.0, 0.0],
        _ => [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
    }
}

/// `outer` applied after `inner`.
pub fn compose(outer: [f32; 6], inner: [f32; 6]) -> [f32; 6] {
    let [a, b, c, d, e, f] = outer;
    let [p, q, r, s, t, u] = inner;
    [a * p + b * s, a * q + b * t, a * r + b * u + c, d * p + e * s, d * q + e * t, d * r + e * u + f]
}

/// A libinput event hook that turns every calibratable device (touchscreens) by `degrees`.
#[cfg(all(target_os = "linux", feature = "backend-linuxkms"))]
pub fn libinput_hook(degrees: u16) -> impl Fn(&input::Event) -> bool + 'static {
    use input::event::{DeviceEvent, EventTrait};
    move |event| {
        if let input::Event::Device(DeviceEvent::Added(added)) = event {
            let mut device = added.device();
            if degrees != 0 && device.config_calibration_has_matrix() {
                let base = device.config_calibration_default_matrix().unwrap_or(touch_matrix(0));
                let result = device.config_calibration_set_matrix(compose(touch_matrix(degrees), base));
                log::info!("touch: {} turned {degrees}° ({result:?})", device.name());
            }
        }
        // Never swallow the event: Slint still handles it.
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(m: [f32; 6], (x, y): (f32, f32)) -> (f32, f32) {
        (m[0] * x + m[1] * y + m[2], m[3] * x + m[4] * y + m[5])
    }

    /// Where Slint's software renderer draws window point (x, y) on the panel, for a window of
    /// `w` × `h` turned clockwise by `degrees` (continuous coordinates).
    fn slint_window_to_panel(degrees: u16, (w, h): (f32, f32), (x, y): (f32, f32)) -> (f32, f32) {
        match degrees {
            90 => (h - y, x),
            180 => (w - x, h - y),
            270 => (y, w - x),
            _ => (x, y),
        }
    }

    #[test]
    fn a_touch_lands_where_slint_drew_it() {
        for degrees in [0, 90, 180, 270] {
            let window = (800.0, 480.0);
            let panel = if degrees % 180 == 90 { (window.1, window.0) } else { window };
            for point in [(0.0, 0.0), (800.0, 0.0), (120.0, 400.0), (640.0, 72.0)] {
                let (px, py) = slint_window_to_panel(degrees, window, point);
                let (u, v) = apply(touch_matrix(degrees), (px / panel.0, py / panel.1));
                let back = (u * window.0, v * window.1);
                assert!(
                    (back.0 - point.0).abs() < 0.01 && (back.1 - point.1).abs() < 0.01,
                    "{degrees}°: {point:?} came back as {back:?}"
                );
            }
        }
    }

    #[test]
    fn rotation_goes_on_top_of_a_udev_calibration() {
        // A touchscreen mounted mirrored left–right, fixed in udev.
        let mirror = [-1.0, 0.0, 1.0, 0.0, 1.0, 0.0];
        let m = compose(touch_matrix(90), mirror);
        // Raw (0.2, 0.3) → mirrored (0.8, 0.3) → turned (0.3, 0.2).
        let (x, y) = apply(m, (0.2, 0.3));
        assert!((x - 0.3).abs() < 1e-6 && (y - 0.2).abs() < 1e-6);
        assert_eq!(compose(touch_matrix(0), mirror), mirror);
    }
}
