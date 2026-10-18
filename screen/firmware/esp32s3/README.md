# xBloom Screen — ESP32-S3 firmware

This is the same app as the Raspberry Pi build: the same UI, admin panel, machine driver and
safety model, all from the shared crates in `../../crates`. Installation and usage are in
[`screen/README.md`](../../README.md); this page covers the hardware side.

> ⚠️ This firmware compiles in CI but **has not been tested on hardware yet**.

## Layout

| File | |
| --- | --- |
| `src/board.rs` | Pin map and panel timings for each supported board (one cargo feature per board) |
| `src/display.rs` | RGB panel via `esp_lcd`: two PSRAM frame buffers, internal-RAM bounce buffers, PWM backlight |
| `src/touch.rs` | GT911 touch over I²C, polled |
| `src/platform.rs` | The Slint platform: software-rendered window, event loop, touch → pointer events |
| `src/ble.rs` | NimBLE central implementing the driver's `Transport` (Write Command to `ffe1`, notifications from `ffe2`) |
| `src/net.rs` | Wi-Fi station + WPA2 setup hotspot, SNTP, captive-portal DNS |
| `src/http.rs` | `esp_http_server` → the shared admin API |
| `src/storage.rs` | FAT + wear levelling on the `storage` partition, mounted at `/data` |

## Memory map

- **Flash (16 MB):** `factory` app 7 MB, `storage` ≈ 8.9 MB (see `partitions.csv`).
- **PSRAM (8 MB):** code and constants run from PSRAM (`CONFIG_SPIRAM_XIP_FROM_PSRAM`), about
  5.4 MB. That keeps the display stable while flash is written, and lets thread stacks live
  in PSRAM. The rest holds the two 750 KB frame buffers and ~1.5 MB of heap: screensaver
  frames, Wi-Fi buffers, the app. Code size is PSRAM, so the release build is optimized for
  size (`opt-level = "z"`).
- **Internal RAM:** the UI task (32 KB stack), Wi-Fi/BLE controllers, DMA bounce buffers.

## Porting to another board

Any ESP32-S3 board with a 16-bit parallel-RGB panel, a GT911, 16 MB flash and 8 MB octal
PSRAM should only need a new `Board` constant in `src/board.rs` behind a cargo feature. A
board with a different touch controller or an SPI panel needs its own driver in
`touch.rs` / `display.rs`. Panels other than 800 × 480 work: the UI lays itself out for
any size from 480 × 272 up, landscape or portrait (see
[Screen sizes](../../README.md#screen-sizes-and-orientation)). A panel mounted on its side
needs no code: set **Rotation** in Settings → Display, and the renderer turns the picture and
`platform.rs` turns the touches to match.

## Toolchain notes

- The Xtensa toolchain is pinned to **1.98.1.0**. Toolchain 1.97 aborts in LLVM on selects
  between two float constants ("Cannot select XtensaISD::PCREL_WRAPPER"), and Slint UIs are
  full of those. Toolchain 1.99's `std` doesn't build for ESP-IDF.
- ESP-IDF **v5.5.6** is downloaded on the first build (set in `.cargo/config.toml`).
