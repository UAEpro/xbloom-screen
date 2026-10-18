//! Networking: Wi-Fi, the clock (SNTP) and a captive-portal DNS for first-time setup.
//!
//! - With Wi-Fi credentials (set in the admin panel), the screen joins that network and the
//!   admin panel is at its DHCP address.
//! - Without them, or while the configured network has been out of reach for a while, the
//!   screen also hosts its own WPA2 hotspot, `xBloom-Screen-XXXX`, with a per-device random
//!   password. The screen shows the name, password and a join QR code; a tiny DNS server then
//!   points every name at the screen so phones pop up the admin panel as a sign-in page.
//!
//! The hotspot is never open: until an admin PIN is set, whoever reaches the panel first could
//! claim the device.

use std::net::{Ipv4Addr, UdpSocket};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sntp::EspSntp;
use esp_idf_svc::sys::EspError;
use esp_idf_svc::wifi::{AccessPointConfiguration, AuthMethod, ClientConfiguration, Configuration, EspWifi};
use serde::{Deserialize, Serialize};
use xbloom_app::settings::Wifi;
use xbloom_app::util::{read_json, write_json};
use xbloom_app::Hotspot;

/// The hotspot's own address (ESP-IDF's default soft-AP network).
pub const AP_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);
/// How long the configured network may be unreachable before the hotspot comes back.
const FALLBACK_AFTER: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Off,
    Station,
    Hotspot,
    Both,
}

#[derive(Default)]
struct State {
    creds: Wifi,
    dirty: bool,
    hotspot_on: bool,
    ip: Option<Ipv4Addr>,
}

#[derive(Serialize, Deserialize)]
struct SavedHotspot {
    ssid: String,
    password: String,
}

/// The network service.
pub struct Network {
    state: Mutex<State>,
    hotspot: Hotspot,
    port: u16,
}

impl Network {
    /// Bring up Wi-Fi with `creds` and keep it up from a background thread.
    pub fn start(
        modem: Modem<'static>,
        sysloop: EspSystemEventLoop,
        nvs: EspDefaultNvsPartition,
        creds: &Wifi,
        data_dir: &Path,
        port: u16,
    ) -> Result<Arc<Self>, EspError> {
        let wifi = EspWifi::new(modem, sysloop, Some(nvs))?;
        let mac = wifi.ap_netif().get_mac().unwrap_or_default();
        let hotspot = hotspot_identity(data_dir, mac);
        let net = Arc::new(Self {
            state: Mutex::new(State { creds: creds.clone(), dirty: true, ..State::default() }),
            hotspot,
            port,
        });
        let sntp = EspSntp::new_default()?;
        let me = Arc::clone(&net);
        thread::Builder::new()
            .name("xbloom-net".into())
            .stack_size(8 * 1024)
            .spawn(move || me.supervise(wifi, sntp))
            .map_err(|_| EspError::from_infallible::<{ esp_idf_svc::sys::ESP_ERR_NO_MEM }>())?;
        Ok(net)
    }

    /// New credentials from the admin panel (applied within a second).
    pub fn set_credentials(&self, creds: &Wifi) {
        let mut s = self.state.lock().unwrap();
        s.creds = creds.clone();
        s.dirty = true;
    }

    /// Where the admin panel can be reached right now.
    pub fn urls(&self) -> Vec<String> {
        let s = self.state.lock().unwrap();
        let port = if self.port == 80 { String::new() } else { format!(":{}", self.port) };
        let mut urls: Vec<String> = s.ip.iter().map(|ip| format!("http://{ip}{port}")).collect();
        if s.hotspot_on {
            urls.push(format!("http://{AP_IP}{port}"));
        }
        urls
    }

    /// The hotspot, while it is the way in (no station connection).
    pub fn setup_hotspot(&self) -> Option<Hotspot> {
        let s = self.state.lock().unwrap();
        (s.hotspot_on && s.ip.is_none()).then(|| self.hotspot.clone())
    }

    /// True while the hotspot is up.
    pub fn hotspot_on(&self) -> bool {
        self.state.lock().unwrap().hotspot_on
    }

    fn supervise(self: Arc<Self>, mut wifi: EspWifi<'static>, _sntp: EspSntp<'static>) {
        let mut mode = Mode::Off;
        let mut down_since = Instant::now();
        let mut next_try = Instant::now();
        let mut backoff = Duration::from_secs(5);
        let mut dns_started = false;
        loop {
            let (creds, dirty) = {
                let mut s = self.state.lock().unwrap();
                (s.creds.clone(), std::mem::take(&mut s.dirty))
            };
            let configured = !creds.ssid.is_empty();
            // Station link with an address (the hotspot side doesn't count).
            let up = configured && mode != Mode::Hotspot && wifi.sta_netif().is_up().unwrap_or(false);
            let now = Instant::now();
            if up || dirty {
                down_since = now;
                backoff = Duration::from_secs(5);
            }
            let want = match (configured, now.duration_since(down_since) > FALLBACK_AFTER) {
                (false, _) => Mode::Hotspot,
                (true, false) => Mode::Station,
                (true, true) => Mode::Both,
            };
            if dirty || want != mode {
                match self.apply(&mut wifi, want, &creds) {
                    Ok(()) => {
                        log::info!("wifi: {want:?}");
                        mode = want;
                        next_try = now;
                    }
                    Err(e) => log::error!("wifi: can't switch to {want:?}: {e}"),
                }
            }
            if configured && !up && now >= next_try {
                if let Err(e) = wifi.connect() {
                    log::debug!("wifi: connect: {e}");
                }
                next_try = now + backoff;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
            let ip = up.then(|| wifi.sta_netif().get_ip_info().ok().map(|i| i.ip)).flatten();
            let hotspot_on = matches!(mode, Mode::Hotspot | Mode::Both);
            {
                let mut s = self.state.lock().unwrap();
                if s.ip != ip {
                    match ip {
                        Some(ip) => log::info!("wifi: online at {ip}"),
                        None => log::info!("wifi: offline"),
                    }
                }
                s.ip = ip;
                s.hotspot_on = hotspot_on;
            }
            if hotspot_on && !dns_started {
                dns_started = thread::Builder::new()
                    .name("xbloom-dns".into())
                    .stack_size(6 * 1024)
                    .spawn(|| {
                        if let Err(e) = captive_dns() {
                            log::warn!("dns: {e}");
                        }
                    })
                    .is_ok();
            }
            thread::sleep(Duration::from_secs(1));
        }
    }

    fn apply(&self, wifi: &mut EspWifi<'static>, mode: Mode, creds: &Wifi) -> Result<(), EspError> {
        let client = || ClientConfiguration {
            ssid: creds.ssid.as_str().try_into().unwrap_or_default(),
            password: creds.password.as_str().try_into().unwrap_or_default(),
            auth_method: if creds.password.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal },
            ..Default::default()
        };
        let ap = || AccessPointConfiguration {
            ssid: self.hotspot.ssid.as_str().try_into().unwrap_or_default(),
            password: self.hotspot.password.as_str().try_into().unwrap_or_default(),
            auth_method: AuthMethod::WPA2Personal,
            channel: 6,
            max_connections: 4,
            ..Default::default()
        };
        let conf = match mode {
            Mode::Off => Configuration::None,
            Mode::Station => Configuration::Client(client()),
            Mode::Hotspot => Configuration::AccessPoint(ap()),
            Mode::Both => Configuration::Mixed(client(), ap()),
        };
        if wifi.is_started().unwrap_or(false) {
            let _ = wifi.disconnect();
        }
        wifi.set_configuration(&conf)?;
        if !wifi.is_started()? {
            wifi.start()?;
        }
        Ok(())
    }
}

/// The hotspot name and password: made once per device and kept on the data partition, so
/// a phone that joined before can rejoin.
fn hotspot_identity(data_dir: &Path, mac: [u8; 6]) -> Hotspot {
    let path = data_dir.join("hotspot.json");
    if let Ok(Some(h)) = read_json::<SavedHotspot>(&path) {
        if h.password.len() >= 8 {
            return Hotspot { ssid: h.ssid, password: h.password };
        }
    }
    // No look-alike characters (l/1, o/0): it's typed from a screen.
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut raw = [0u8; 10];
    getrandom::fill(&mut raw).expect("hardware RNG");
    let password: String = raw.iter().map(|b| ALPHABET[usize::from(*b) % ALPHABET.len()] as char).collect();
    let ssid = format!("xBloom-Screen-{:02X}{:02X}", mac[4], mac[5]);
    let _ = write_json(&path, &SavedHotspot { ssid: ssid.clone(), password: password.clone() });
    Hotspot { ssid, password }
}

/// Answer every DNS question on the hotspot with the screen's own address.
fn captive_dns() -> std::io::Result<()> {
    // The hotspot's interface may still be coming up.
    let sock = loop {
        match UdpSocket::bind((AP_IP, 53)) {
            Ok(s) => break s,
            Err(_) => thread::sleep(Duration::from_secs(2)),
        }
    };
    log::info!("dns: captive portal on {AP_IP}:53");
    let mut buf = [0u8; 512];
    loop {
        let (n, peer) = sock.recv_from(&mut buf)?;
        if let Some(reply) = dns_reply(&buf[..n], AP_IP) {
            let _ = sock.send_to(&reply, peer);
        }
    }
}

/// A DNS response for a single-question query: `A` questions get `ip`, others an empty answer.
fn dns_reply(q: &[u8], ip: Ipv4Addr) -> Option<Vec<u8>> {
    if q.len() < 12 || q[2] & 0x80 != 0 || u16::from_be_bytes([q[4], q[5]]) != 1 {
        return None;
    }
    let mut i = 12;
    while *q.get(i)? != 0 {
        i += usize::from(q[i]) + 1;
    }
    let end = i + 5; // zero label + QTYPE + QCLASS
    let qtype = u16::from_be_bytes([*q.get(i + 1)?, *q.get(i + 2)?]);
    q.get(end - 1)?;
    let is_a = qtype == 1;
    let mut r = Vec::with_capacity(end + 16);
    r.extend_from_slice(&q[..2]);
    r.extend_from_slice(&[0x80 | (q[2] & 0x01), 0x80]); // response, RD copied, RA
    r.extend_from_slice(&[0, 1, 0, u8::from(is_a), 0, 0, 0, 0]);
    r.extend_from_slice(&q[12..end]);
    if is_a {
        // Name pointer to the question, type A, class IN, TTL 60 s, 4 bytes.
        r.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
        r.extend_from_slice(&ip.octets());
    }
    Some(r)
}
