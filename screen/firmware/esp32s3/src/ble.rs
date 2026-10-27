//! The machine link on the ESP32: a NimBLE central implementing the shared driver's
//! [`Transport`] — scan, connect, write `ffe1` (Write Command), receive `ffe2` notifications.
//!
//! The Raspberry Pi app does the same over BlueZ; everything above this trait (the xBloom
//! conversation, the 🛑 load-only / explicit-start split) is the shared `xbloom-core` driver.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use esp32_nimble::utilities::BleUuid;
use esp32_nimble::{BLEAddress, BLEAddressType, BLEClient, BLEDevice, BLEScan};
use esp_idf_svc::hal::task::block_on;
use xbloom_core::driver::{is_xbloom, Discovered, Transport, TransportError, CHAR_COMMAND, CHAR_STATUS, SERVICE_UUID};

fn uuid(s: &str) -> BleUuid {
    BleUuid::from_uuid128_string(s).expect("valid UUID constant")
}

fn ble_err(e: impl core::fmt::Debug) -> TransportError {
    TransportError::Ble(format!("{e:?}"))
}

/// NimBLE transport (one machine at a time).
pub struct NimbleTransport {
    client: Option<BLEClient>,
    alive: Arc<AtomicBool>,
    tx: Sender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
    /// Address types seen while scanning (an address alone doesn't say public vs random).
    kinds: HashMap<String, u8>,
}

// SAFETY: the transport is created on the UI thread and from then on used only by the
// machine-controller thread; NimBLE's own callbacks touch only the channel and the flag.
unsafe impl Send for NimbleTransport {}

impl NimbleTransport {
    /// Start the NimBLE host (central role).
    pub fn new() -> Self {
        let device = BLEDevice::take();
        // As large as ATT allows: a pours frame can be ~260 bytes and must fit one packet.
        if let Err(e) = device.set_preferred_mtu(517) {
            log::warn!("ble: preferred MTU: {e:?}");
        }
        let (tx, rx) = mpsc::channel();
        Self { client: None, alive: Arc::new(AtomicBool::new(false)), tx, rx, kinds: HashMap::new() }
    }

    fn write_frame(client: &mut BLEClient, frame: &[u8]) -> Result<(), TransportError> {
        // The machine only accepts Write Command (write-without-response), which must fit in
        // one packet: NimBLE would quietly turn a longer one into a long *Write Request*, which
        // the machine rejects. (The MTU is negotiated on connect.)
        let mtu = client.desc().map(|d| usize::from(d.mtu())).map_err(ble_err)?;
        if frame.len() + 3 > mtu {
            return Err(TransportError::Ble(format!("{}-byte frame exceeds the link MTU ({mtu})", frame.len())));
        }
        block_on(async {
            let service = client.get_service(uuid(SERVICE_UUID)).await.map_err(ble_err)?;
            let chr = service.get_characteristic(uuid(CHAR_COMMAND)).await.map_err(ble_err)?;
            chr.write_value(frame, false).await.map_err(ble_err)
        })
    }
}

impl Transport for NimbleTransport {
    fn scan(&mut self, timeout: Duration) -> Result<Vec<Discovered>, TransportError> {
        let device = BLEDevice::take();
        let mut found: Vec<Discovered> = Vec::new();
        let mut kinds: Vec<(String, u8)> = Vec::new();
        let service = uuid(SERVICE_UUID);
        let ms = timeout.as_millis().min(30_000) as i32;
        let mut scan = BLEScan::new();
        scan.active_scan(true).interval(100).window(99);
        block_on(scan.start(device, ms, |dev, data| {
            let name = data.name().map(|n| n.to_string());
            let uuids: Vec<String> =
                if data.is_advertising_service(&service) { vec![SERVICE_UUID.to_string()] } else { Vec::new() };
            if is_xbloom(name.as_deref(), &uuids) {
                let address = dev.addr().to_string();
                if let Some(d) = found.iter_mut().find(|d| d.address == address) {
                    d.rssi = Some(i16::from(dev.rssi()));
                    d.name = d.name.take().or(name);
                } else {
                    kinds.push((address.clone(), dev.addr().addr_type() as u8));
                    found.push(Discovered { address, name, rssi: Some(i16::from(dev.rssi())) });
                }
            }
            None::<()>
        }))
        .map_err(ble_err)?;
        self.kinds.extend(kinds);
        found.sort_by_key(|d| -d.rssi.unwrap_or(-127));
        Ok(found)
    }

    fn connect(&mut self, address: &str) -> Result<(), TransportError> {
        self.disconnect();
        while self.rx.try_recv().is_ok() {}
        let kinds = match self.kinds.get(address) {
            Some(k) => vec![*k],
            None => vec![BLEAddressType::Public as u8, BLEAddressType::Random as u8],
        };
        let device = BLEDevice::take();
        let mut last = TransportError::NotFound(address.to_string());
        for kind in kinds {
            let kind = BLEAddressType::try_from(kind).unwrap_or(BLEAddressType::Public);
            let Some(addr) = BLEAddress::from_str(address, kind) else {
                return Err(TransportError::NotFound(format!("bad address {address}")));
            };
            let mut client = device.new_client();
            let alive = Arc::clone(&self.alive);
            client.on_disconnect(move |reason| {
                log::info!("ble: disconnected (reason {reason})");
                alive.store(false, Ordering::Release);
            });
            let tx = self.tx.clone();
            let result: Result<(), TransportError> = block_on(async {
                client.connect(&addr).await.map_err(ble_err)?;
                let service = client
                    .get_service(uuid(SERVICE_UUID))
                    .await
                    .map_err(|_| TransportError::NotFound("xBloom service".into()))?;
                let status = service
                    .get_characteristic(uuid(CHAR_STATUS))
                    .await
                    .map_err(|_| TransportError::NotFound("status characteristic".into()))?;
                status.on_notify(move |data| {
                    let _ = tx.send(data.to_vec());
                });
                status.subscribe_notify(true).await.map_err(ble_err)?;
                Ok(())
            });
            match result {
                Ok(()) => {
                    self.alive.store(true, Ordering::Release);
                    self.client = Some(client);
                    log::info!("ble: connected to {address}");
                    return Ok(());
                }
                Err(e) => {
                    let _ = client.disconnect();
                    last = e;
                }
            }
        }
        Err(last)
    }

    fn disconnect(&mut self) {
        if let Some(mut c) = self.client.take() {
            if c.connected() {
                let _ = c.disconnect();
            }
        }
        self.alive.store(false, Ordering::Release);
    }

    fn is_connected(&self) -> bool {
        self.alive.load(Ordering::Acquire) && self.client.as_ref().is_some_and(BLEClient::connected)
    }

    fn write(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        if !self.is_connected() {
            return Err(TransportError::NotConnected);
        }
        let client = self.client.as_mut().ok_or(TransportError::NotConnected)?;
        Self::write_frame(client, frame)
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, TransportError> {
        if self.client.is_none() {
            return Err(TransportError::NotConnected);
        }
        // Frames that arrived before a disconnect are still delivered first.
        match self.rx.recv_timeout(timeout) {
            Ok(frame) => Ok(Some(frame)),
            Err(RecvTimeoutError::Timeout) if self.is_connected() => Ok(None),
            Err(_) => Err(TransportError::Disconnected),
        }
    }
}
