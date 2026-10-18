//! The data partition: FAT with wear levelling on the `storage` flash partition, mounted at
//! [`DATA_DIR`]. Settings, recipes, history, presets and screensaver media live here, exactly
//! as in the Raspberry Pi app's data directory.

use std::ffi::CStr;

use esp_idf_svc::sys::{self, esp, EspError};

/// Where the data partition is mounted.
pub const DATA_DIR: &str = "/data";
const MOUNT: &CStr = c"/data";
const PARTITION: &CStr = c"storage";

/// Mount the data partition, formatting it on first boot (or if it is unreadable).
pub fn mount() -> Result<(), EspError> {
    let cfg = sys::esp_vfs_fat_mount_config_t {
        format_if_mount_failed: true,
        max_files: 8,
        allocation_unit_size: 4096,
        disk_status_check_enable: false,
        use_one_fat: false,
    };
    let mut wl: sys::wl_handle_t = sys::WL_INVALID_HANDLE as _;
    // SAFETY: static C strings and a valid config; the handle is written by the call.
    esp!(unsafe { sys::esp_vfs_fat_spiflash_mount_rw_wl(MOUNT.as_ptr(), PARTITION.as_ptr(), &cfg, &mut wl) })?;
    if let Some((free, total)) = usage() {
        log::info!("storage: {DATA_DIR} mounted, {} KB free of {} KB", free / 1024, total / 1024);
    }
    Ok(())
}

/// `(free, total)` bytes.
pub fn usage() -> Option<(u64, u64)> {
    let (mut total, mut free) = (0u64, 0u64);
    // SAFETY: the mount point string is static; the call writes both counters.
    esp!(unsafe { sys::esp_vfs_fat_info(MOUNT.as_ptr(), &mut total, &mut free) }).ok()?;
    Some((free, total))
}
