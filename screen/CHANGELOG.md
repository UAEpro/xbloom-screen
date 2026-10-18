# xBloom Screen changelog

The touchscreen controller in `screen/` is versioned separately from the `xbloom-ble`
Python package. Releases are tagged `screen-vX.Y.Z` (see
[docs/RELEASING.md](../docs/RELEASING.md#xbloom-screen-releases)); the rolling
`screen-latest` pre-release always holds the newest build of `main`.

## Unreleased

### The screen

- **Tea recipes.** A recipe can be coffee or tea. Tea brews with the grinder off, so the
  leaves go in a paper filter. Green, black and oolong starter teas come with the coffee
  starters, and a filter on the home screen shows all recipes, coffee only or tea only. Tea
  cards show water, temperature and time instead of dose and grind.
- **Rinse the filter.** A Rinse button on the home screen pours hot water through the empty
  filter and dripper with the grinder off (100 ml at 95 °C by default, set in Settings →
  Machine). It starts with the same hold gesture as a brew and isn't added to the history.
- **Timers.** Brew at a set time, once or on chosen weekdays. A timer never brews on its
  own: each run is armed at the machine, after a checklist (beans or leaves, filter and
  dripper, cup, water) and **Hold to arm**. When the run is due, the screen counts down for
  a minute with **Cancel**. A late run (screen off) or one that clashes with a running brew
  is skipped, not brewed late, and editing the timer or its recipe disarms it. An optional
  evening reminder asks you to arm the next morning's timer. The new Timers page lists the
  timers and the next run.

### The admin panel

- A **Timers** page: add, edit, switch off and delete timers, see the next run, and disarm
  it or cancel its countdown. The API has no way to arm a timer or start a brew.
- The recipe editor has a **Coffee / Tea** type. A tea recipe's grind is off, and its dose
  (bookkeeping for the machine's ratio check) follows its water.
- Rinse water, rinse temperature and the timer reminder in Settings → Machine.
- Fixes: on a phone, the top bar could make every page scroll sideways; Enter now submits
  the two-field PIN setup form.

### Recipes

- A brew ratio above **1:25.5** is rejected, in the Python package and here: the machine
  stores the ratio ×10 in one byte, so more water per gram doesn't fit.

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
