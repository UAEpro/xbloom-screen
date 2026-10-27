# xBloom Screen — ESP32-S3 firmware

This is the same app as the Raspberry Pi build: the same UI, admin panel, machine driver and
safety model, all from the shared crates in `../../crates`. Installation and usage are in
[`screen/README.md`](../../README.md); this page covers the hardware side.

> ⚠️ This firmware compiles in CI but **has not been tested on hardware yet**.

## Layout

| File | |
| --- | --- |
| `src/board.rs` | Pin map and panel timings for each supported board (one cargo feature per board) |
| `src/io.rs` | Control lines (resets, backlight enable): GPIOs, or pins on a CH422G I/O expander |
| `src/display.rs` | RGB panel via `esp_lcd`: two PSRAM frame buffers, internal-RAM bounce buffers; PWM or on/off backlight |
| `src/touch.rs` | GT911 touch over I²C, polled |
| `src/platform.rs` | The Slint platform: software-rendered window, event loop, touch → pointer events |
| `src/ble.rs` | NimBLE central implementing the driver's `Transport` (Write Command to `ffe1`, notifications from `ffe2`) |
| `src/net.rs` | Wi-Fi station + WPA2 setup hotspot, SNTP, captive-portal DNS |
| `src/http.rs` | `esp_http_server` → the shared admin API |
| `src/storage.rs` | FAT + wear levelling on the `storage` partition, mounted at `/data` |

## Boards

| Cargo feature | Board | Images |
| --- | --- | --- |
| `board-sunton-8048s043` (default) | Sunton ESP32-8048S043C, 4.3" | `xbloom-screen-esp32s3-sunton-8048s043c-{16mb,8mb}.bin` |
| `board-waveshare-s3-touch-lcd-7` | Waveshare ESP32-S3-Touch-LCD-7, 7" | `xbloom-screen-esp32s3-waveshare-touch-lcd-7-{16mb,8mb}.bin` |

Build one with `cargo build --release --no-default-features --features <feature>` (the
default board needs no flags). CI builds each board once and packages it twice, for 16 MB
(N16R8) and 8 MB (N8R8) modules; see the memory map.

The Waveshare board spends almost every GPIO on the panel, so its backlight enable, panel
reset and touch reset are pins on a CH422G I/O expander that shares the touch I²C bus
(GPIO8/9). Its EXIO5 selects USB or CAN on GPIO19/20; the firmware holds it low so the USB-C
port keeps working.

## Memory map

- **Flash, 16 MB:** `factory` app 7 MB, `storage` ≈ 8.9 MB (`partitions.csv`).
- **Flash, 8 MB:** `factory` app 6 MB, `storage` ≈ 1.9 MB (`partitions-8mb.csv`). The app
  binary is the same: the flash size is a field in the bootloader header, which
  `espflash save-image --flash-size` sets, and the app checks it against the chip at boot.
  The app must stay under 6 MB (it is ~5.4 MB). With the small data partition the admin
  panel accepts 1 MB uploads and 10 s videos instead of 4 MB and 30 s.
- **PSRAM (8 MB):** code and constants run from PSRAM (`CONFIG_SPIRAM_XIP_FROM_PSRAM`), about
  5.4 MB. That keeps the display stable while flash is written, and lets thread stacks live
  in PSRAM. The rest holds the two 750 KB frame buffers and ~1.5 MB of heap: screensaver
  frames, Wi-Fi buffers, the app. Code size is PSRAM, so the release build is optimized for
  size (`opt-level = "z"`).
- **Internal RAM:** Wi-Fi/BLE controllers, DMA bounce buffers, and an 8 KB main task that only
  sets up NVS and the data partition, then starts the UI thread. The UI thread's 64 KB stack is
  in PSRAM: the software renderer needs ~32 KB, more than internal RAM can spare next to Wi-Fi
  and Bluetooth. Flash writes from PSRAM stacks are fine here (the cache stays on), but
  *mapping* flash freezes the cache, so the first partition lookup stays on the main task.
  Plain `malloc()` goes to PSRAM first (`CONFIG_SPIRAM_MALLOC_ALWAYSINTERNAL=0`): Slint allocates
  a small node per property a binding reads, and thousands of those used up internal RAM.

## Porting to another board

Any ESP32-S3 board with a 16-bit parallel-RGB panel, a GT911, 16 MB flash and 8 MB octal
PSRAM should only need a new `Board` constant in `src/board.rs` behind a cargo feature, plus
a row in the CI matrix (`.github/workflows/screen.yml`). Control lines on a CH422G expander
are already handled (`io.rs`); another expander chip needs a few lines there. A board with a
different touch controller or an SPI panel needs its own driver in `touch.rs` /
`display.rs`. Panels other than 800 × 480 work: the UI lays itself out for
any size from 480 × 272 up, landscape or portrait (see
[Screen sizes](../../README.md#screen-sizes-and-orientation)). A panel mounted on its side
needs no code: set **Rotation** in Settings → Display, and the renderer turns the picture and
`platform.rs` turns the touches to match.

## Toolchain notes

- The Xtensa toolchain is pinned to **1.98.1.0**. Toolchain 1.97 aborts in LLVM on selects
  between two float constants ("Cannot select XtensaISD::PCREL_WRAPPER"), and Slint UIs are
  full of those. Toolchain 1.99's `std` doesn't build for ESP-IDF.
- ESP-IDF **v5.5.5** is downloaded on the first build (set in `.cargo/config.toml`). Not v5.5.6:
  it adds a Wi-Fi init field (`wifi_task_stack_size`) that esp-idf-svc 0.53 leaves at 0, and the
  firmware then boot-loops with `wifi: ESP_ERR_NO_MEM`.
