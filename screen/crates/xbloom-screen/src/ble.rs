//! The Linux BLE link: a blocking [`Transport`] over `btleplug` (BlueZ via D-Bus).
//!
//! `btleplug` is async; the driver is blocking, so this owns a small Tokio runtime and blocks
//! on each operation. Notifications from `ffe2` are forwarded by a background task into a
//! channel that [`Transport::recv`] reads; a second task watches adapter events so a dropped
//! link surfaces as [`TransportError::Disconnected`] instead of silence.
//!
//! `btleplug` also runs on macOS and Windows, so the screen app can drive a real machine from
//! a laptop during development.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use btleplug::api::{Central, CentralEvent, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;
use xbloom_core::driver::{is_xbloom, Discovered, Transport, TransportError, CHAR_COMMAND, CHAR_STATUS};

fn ble(e: impl std::fmt::Display) -> TransportError {
    TransportError::Ble(e.to_string())
}

struct Link {
    peripheral: Peripheral,
    command: Characteristic,
    rx: Receiver<Vec<u8>>,
    alive: Arc<AtomicBool>,
    tasks: Vec<JoinHandle<()>>,
}

/// BlueZ (or CoreBluetooth / WinRT) transport.
pub struct BtleTransport {
    rt: Runtime,
    adapter: Option<Adapter>,
    link: Option<Link>,
}

impl BtleTransport {
    /// Create the transport (the adapter is opened lazily, so a box with Bluetooth off still
    /// boots into the UI and demo mode).
    pub fn new() -> std::io::Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("xbloom-ble")
            .enable_all()
            .build()?;
        Ok(Self { rt, adapter: None, link: None })
    }

    fn adapter(&mut self) -> Result<Adapter, TransportError> {
        if let Some(a) = &self.adapter {
            return Ok(a.clone());
        }
        let adapter = self.rt.block_on(async {
            let manager = Manager::new().await.map_err(ble)?;
            manager
                .adapters()
                .await
                .map_err(ble)?
                .into_iter()
                .next()
                .ok_or_else(|| TransportError::NotFound("no Bluetooth adapter (is bluetoothd running?)".into()))
        })?;
        self.adapter = Some(adapter.clone());
        Ok(adapter)
    }

    async fn find(adapter: &Adapter, address: &str, timeout: Duration) -> Result<Peripheral, TransportError> {
        let want = address.to_ascii_uppercase();
        let matches =
            |p: &Peripheral| p.address().to_string().to_ascii_uppercase() == want || p.id().to_string() == address;
        if let Some(p) = adapter.peripherals().await.map_err(ble)?.into_iter().find(matches) {
            return Ok(p);
        }
        adapter.start_scan(ScanFilter::default()).await.map_err(ble)?;
        let deadline = tokio::time::Instant::now() + timeout;
        let found = loop {
            if let Some(p) = adapter.peripherals().await.map_err(ble)?.into_iter().find(matches) {
                break Some(p);
            }
            if tokio::time::Instant::now() >= deadline {
                break None;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        };
        let _ = adapter.stop_scan().await;
        found.ok_or_else(|| TransportError::NotFound(address.to_string()))
    }
}

impl Transport for BtleTransport {
    fn scan(&mut self, timeout: Duration) -> Result<Vec<Discovered>, TransportError> {
        let adapter = self.adapter()?;
        self.rt.block_on(async {
            adapter.start_scan(ScanFilter::default()).await.map_err(ble)?;
            tokio::time::sleep(timeout).await;
            let _ = adapter.stop_scan().await;
            let mut out = Vec::new();
            for p in adapter.peripherals().await.map_err(ble)? {
                let Ok(Some(props)) = p.properties().await else { continue };
                let name = props.local_name.or(props.advertisement_name);
                let services: Vec<String> = props.services.iter().map(ToString::to_string).collect();
                if is_xbloom(name.as_deref(), &services) {
                    out.push(Discovered { address: p.address().to_string(), name, rssi: props.rssi });
                }
            }
            out.sort_by_key(|d| std::cmp::Reverse(d.rssi.unwrap_or(i16::MIN)));
            Ok(out)
        })
    }

    fn connect(&mut self, address: &str) -> Result<(), TransportError> {
        self.disconnect();
        let adapter = self.adapter()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let alive = Arc::new(AtomicBool::new(false));
        let link = self.rt.block_on(async {
            let p = Self::find(&adapter, address, Duration::from_secs(12)).await?;
            p.connect_with_timeout(Duration::from_secs(15)).await.map_err(ble)?;
            p.discover_services_with_timeout(Duration::from_secs(15)).await.map_err(ble)?;
            let chars = p.characteristics();
            let by_uuid = |u: &str| chars.iter().find(|c| c.uuid.to_string().eq_ignore_ascii_case(u)).cloned();
            let command =
                by_uuid(CHAR_COMMAND).ok_or_else(|| TransportError::NotFound("command characteristic ffe1".into()))?;
            let status =
                by_uuid(CHAR_STATUS).ok_or_else(|| TransportError::NotFound("status characteristic ffe2".into()))?;
            let mut notes = p.notifications().await.map_err(ble)?;
            p.subscribe(&status).await.map_err(ble)?;
            alive.store(true, Ordering::Release);

            // ffe2 notifications → channel. Ends (dropping `tx`) when the stream ends.
            let status_uuid = status.uuid;
            let alive_n = Arc::clone(&alive);
            let notify_task = tokio::spawn(async move {
                while let Some(n) = notes.next().await {
                    if n.uuid == status_uuid && tx.send(n.value).is_err() {
                        break;
                    }
                }
                alive_n.store(false, Ordering::Release);
            });
            // Adapter events: mark the link dead when this peripheral disconnects.
            let id = p.id();
            let alive_e = Arc::clone(&alive);
            let mut events = adapter.events().await.map_err(ble)?;
            let event_task = tokio::spawn(async move {
                while let Some(ev) = events.next().await {
                    if matches!(ev, CentralEvent::DeviceDisconnected(ref d) if *d == id) {
                        alive_e.store(false, Ordering::Release);
                        break;
                    }
                }
            });
            Ok::<_, TransportError>(Link {
                peripheral: p,
                command,
                rx,
                alive: Arc::clone(&alive),
                tasks: vec![notify_task, event_task],
            })
        })?;
        self.link = Some(link);
        Ok(())
    }

    fn disconnect(&mut self) {
        if let Some(link) = self.link.take() {
            for t in &link.tasks {
                t.abort();
            }
            let _ = self
                .rt
                .block_on(async { tokio::time::timeout(Duration::from_secs(5), link.peripheral.disconnect()).await });
        }
    }

    fn is_connected(&self) -> bool {
        self.link.as_ref().is_some_and(|l| l.alive.load(Ordering::Acquire))
    }

    fn write(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        let link = self.link.as_ref().ok_or(TransportError::NotConnected)?;
        if !link.alive.load(Ordering::Acquire) {
            return Err(TransportError::Disconnected);
        }
        // ffe1 only accepts Write Command (write-without-response); a Write Request is
        // rejected by the firmware.
        self.rt
            .block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(5),
                    link.peripheral.write(&link.command, frame, WriteType::WithoutResponse),
                )
                .await
            })
            .map_err(|_| TransportError::Ble("write timed out".into()))?
            .map_err(ble)
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, TransportError> {
        let link = self.link.as_ref().ok_or(TransportError::NotConnected)?;
        let r = if timeout.is_zero() {
            link.rx.try_recv().map_err(|e| match e {
                mpsc::TryRecvError::Empty => RecvTimeoutError::Timeout,
                mpsc::TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
            })
        } else {
            link.rx.recv_timeout(timeout)
        };
        match r {
            Ok(f) => Ok(Some(f)),
            Err(RecvTimeoutError::Timeout) if link.alive.load(Ordering::Acquire) => Ok(None),
            Err(_) => Err(TransportError::Disconnected),
        }
    }
}

impl Drop for BtleTransport {
    fn drop(&mut self) {
        self.disconnect();
    }
}
