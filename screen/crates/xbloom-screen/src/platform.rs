//! Linux platform hooks: backlight, restart, storage, network addresses.

use std::ffi::CStr;
use std::path::{Path, PathBuf};

use xbloom_app::Platform;

/// Hooks for a Raspberry Pi (or any Linux box).
pub struct LinuxPlatform {
    data_dir: PathBuf,
    port: u16,
    backlight: Option<PathBuf>,
}

impl LinuxPlatform {
    /// Hooks for an app storing data in `data_dir` and serving the admin panel on `port`.
    pub fn new(data_dir: &Path, port: u16) -> Self {
        let backlight = std::fs::read_dir("/sys/class/backlight")
            .ok()
            .and_then(|mut d| d.next())
            .and_then(Result::ok)
            .map(|e| e.path());
        if let Some(b) = &backlight {
            log::info!("backlight: {}", b.display());
        }
        Self { data_dir: data_dir.to_path_buf(), port, backlight }
    }
}

/// IPv4 addresses of the non-loopback interfaces that are up.
pub fn ipv4_addresses() -> Vec<String> {
    let mut out = Vec::new();
    // SAFETY: getifaddrs allocates a linked list we walk read-only and then free.
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null()
                && i32::from((*ifa.ifa_addr).sa_family) == libc::AF_INET
                && ifa.ifa_flags & (libc::IFF_UP as u32) != 0
                && ifa.ifa_flags & (libc::IFF_LOOPBACK as u32) == 0
            {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                out.push(ip.to_string());
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out
}

fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes a NUL-terminated name into the buffer.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    (rc == 0).then(|| CStr::from_bytes_until_nul(&buf).ok().map(|c| c.to_string_lossy().into_owned())).flatten()
}

impl Platform for LinuxPlatform {
    fn restart(&self) {
        // systemd (Restart=always) brings the app straight back.
        log::info!("restart requested");
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(400));
            std::process::exit(0);
        });
    }

    fn set_brightness(&self, percent: u8) {
        let Some(dir) = &self.backlight else { return };
        let max: u32 =
            std::fs::read_to_string(dir.join("max_brightness")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(255);
        let value = if percent == 0 { 0 } else { (max * u32::from(percent.min(100)) / 100).max(1) };
        if let Err(e) = std::fs::write(dir.join("brightness"), value.to_string()) {
            log::debug!("backlight: {e} (add the user to the video group / install the udev rule)");
        }
        let _ = std::fs::write(dir.join("bl_power"), if percent == 0 { "1" } else { "0" });
    }

    fn storage(&self) -> Option<(u64, u64)> {
        let path = std::ffi::CString::new(self.data_dir.to_string_lossy().as_bytes()).ok()?;
        // SAFETY: statvfs fills the struct for a valid NUL-terminated path.
        unsafe {
            let mut st: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(path.as_ptr(), &mut st) != 0 {
                return None;
            }
            let f = st.f_frsize as u64;
            Some((st.f_bavail as u64 * f, st.f_blocks as u64 * f))
        }
    }

    fn admin_urls(&self) -> Vec<String> {
        let port = if self.port == 80 { String::new() } else { format!(":{}", self.port) };
        let mut urls: Vec<String> = ipv4_addresses().into_iter().map(|ip| format!("http://{ip}{port}")).collect();
        if let Some(h) = hostname() {
            urls.push(format!("http://{h}.local{port}"));
        }
        urls
    }
}
