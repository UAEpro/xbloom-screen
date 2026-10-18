# xBloom Screen changelog

The touchscreen controller in `screen/` is versioned separately from the `xbloom-ble`
Python package. Releases are tagged `screen-vX.Y.Z` (see
[docs/RELEASING.md](../docs/RELEASING.md#xbloom-screen-releases)); the rolling
`screen-latest` pre-release always holds the newest build of `main`.

## 0.1.0 (2026-10-18)

The first release.

### The screen

- Recipe library, recipe details with a dose stepper, and **Load** (arms the machine; you
  press start on it) or the optional **Hold to brew** gesture.
- Live brew view: progress ring, water / in-cup chart, pour stages and **Stop**, then the
  result with **Brew again**.
- Brew history with curves; the machine's A · B · C dial presets.
- Lock: off, PIN to brew, or PIN for the whole screen, with a separate user PIN and
  re-locking after idle.
- Screensaver: clock, video or photo slideshow, dimming, then sleep.
- Light, dark or automatic theme, six accent colours, brightness, 12/24-hour clock.
- Setup wizard (admin PIN → pair the machine or demo mode → the admin panel QR code), and
  a demo mode with a built-in simulated machine.
- **Any screen size**: the layout adapts from 480 × 272 to large HDMI panels, landscape or
  portrait. Stop, the brew controls and every dialog stay on screen. **Interface size**
  (70–200 %) and **rotation** (0/90/180/270°, picture and touch) settings.

### The admin panel (phone or laptop)

- Recipe editor with live validation, YAML import / export (the `xbloom` CLI format),
  reorder and hide.
- Dial presets, screensaver media (videos converted in the browser), history with CSV
  export, all settings, backup / restore, restart.
- PINs stored as salted PBKDF2 hashes, throttled login, `HttpOnly` / `SameSite=Strict`
  session cookie, CSRF header, strict Content-Security-Policy. The API has no
  brew-start endpoint.

### Targets

- **Raspberry Pi** (Zero 2 W, 3B+, 4, 5; 64-bit Raspberry Pi OS Lite): draws straight to
  DRM/KMS, BlueZ for Bluetooth, systemd service and one-line installer.
- **ESP32-S3**: Sunton ESP32-8048S043C (4.3") or Waveshare ESP32-S3-Touch-LCD-7 (7"), both
  800 × 480, with 16 MB (N16R8) or 8 MB (N8R8) flash; one flashable image per board and
  flash size. Setup Wi-Fi hotspot with a join QR code, captive portal, SNTP clock. **Experimental: built in CI, not
  yet tested on hardware.**

### Under the hood

- Rust port of the protocol, recipe validation and telemetry, held byte-for-byte to the
  Python package by golden vectors that CI regenerates and compares.
- The load path never carries the commit / start / cancel opcodes (`0x42` / `0x46` /
  `0x47`), with tests mirroring the Python safety tests.
