//! Matter commissioning transport over Bluetooth LE: the BTP central role
//! (rs-matter 0.2 only implements the peripheral) on top of btleplug GATT.
//!
//! [`btp::BtpCentral`] is the pure protocol; [`BleTransport`] pumps it
//! between rs-matter's [`NetworkSend`]/[`NetworkReceive`] and a [`GattLink`]
//! (btleplug in production, a simulated link in tests).

use core::future::Future;
use core::pin::{pin, Pin};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use btleplug::api::{
    Central, CentralEvent, CentralState, Characteristic, Manager as _, Peripheral as _, ScanFilter,
    ValueNotification, WriteType,
};
use btleplug::platform::{Manager, Peripheral, PeripheralId};
use futures_lite::{Stream, StreamExt};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::transport::network::btp::{
    C1_CHARACTERISTIC_UUID, C2_CHARACTERISTIC_UUID, MATTER_BLE_SERVICE_UUID,
};
use rs_matter::transport::network::{Address, BtAddr, NetworkReceive, NetworkSend};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use uuid::Uuid;

pub mod btp;

use btp::BtpCentral;

/// How long the peer has to answer the BTP handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Which advertisement to connect to: full 12-bit discriminator, or the 4-bit
/// short one from a manual pairing code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discriminator {
    Long(u16),
    Short(u8),
}

impl Discriminator {
    /// Whether an advertised 12-bit discriminator matches.
    pub fn matches(&self, advertised: u16) -> bool {
        let advertised = advertised & 0x0FFF;
        match *self {
            Self::Long(d) => advertised == d & 0x0FFF,
            Self::Short(s) => (advertised >> 8) as u8 == s & 0x0F,
        }
    }
}

/// A commissionable Matter BLE advertisement (Core spec 5.4.2.5.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Advertisement {
    pub discriminator: u16,
    pub version: u8,
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
}

/// Parses the service data advertised under the Matter service UUID 0xFFF6:
/// opcode 0x00 (commissionable), then a little-endian u16 with the
/// discriminator in bits 0-11 and the advertisement version in bits 12-15,
/// then vendor and product IDs.
pub fn parse_service_data(data: &[u8]) -> Option<Advertisement> {
    if data.len() < 3 || data[0] != 0x00 {
        return None;
    }
    let word = u16::from_le_bytes([data[1], data[2]]);
    let u16_at = |i: usize| data.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    Some(Advertisement {
        discriminator: word & 0x0FFF,
        version: (word >> 12) as u8,
        vendor_id: u16_at(3),
        product_id: u16_at(5),
    })
}

/// A boxed, non-`Send` future returned by [`GattLink`].
pub type LinkFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + 'a>>;

/// The GATT operations BTP needs from a connected Matter peripheral.
pub trait GattLink {
    /// Write-with-response to C1.
    fn write_c1<'a>(&'a self, data: &'a [u8]) -> LinkFuture<'a, ()>;
    /// Enable indications on C2.
    fn subscribe_c2(&self) -> LinkFuture<'_, ()>;
    /// The next C2 indication; an error once the link is gone.
    fn next_c2(&self) -> LinkFuture<'_, Vec<u8>>;
    /// Drop the connection.
    fn disconnect(&self) -> LinkFuture<'_, ()>;
}

struct State {
    core: BtpCentral,
    closed: Option<String>,
}

/// A BTP session with one commissionable device, usable as an rs-matter
/// network for `Address::Btp(transport.address())`.
pub struct BleTransport {
    link: Box<dyn GattLink>,
    address: BtAddr,
    state: Mutex<State>,
    /// Something for the writer to do: data queued, segment received, closed.
    tx_wake: Notify,
    /// A reassembled message arrived, or closed.
    rx_wake: Notify,
    /// Send queue space freed, or closed.
    space_wake: Notify,
}

impl BleTransport {
    /// Scans for a commissionable Matter device whose discriminator matches,
    /// connects, discovers C1/C2 and performs the BTP handshake (write the
    /// request to C1, subscribe to C2, await the response indication), so a
    /// returned transport is ready for Matter traffic once [`Self::run`] is
    /// polled. `timeout` bounds the scan; connecting and the handshake have
    /// their own fixed limits.
    pub async fn open(discriminator: Discriminator, timeout: Duration) -> Result<Self> {
        let (link, address) = BtleplugLink::connect(discriminator, timeout).await?;
        Self::with_link(Box::new(link), address, HANDSHAKE_TIMEOUT).await
    }

    /// Performs the BTP handshake over an already-connected link.
    pub async fn with_link(
        link: Box<dyn GattLink>,
        address: BtAddr,
        handshake_timeout: Duration,
    ) -> Result<Self> {
        let handshake = async {
            link.write_c1(&btp::handshake_request())
                .await
                .context("writing BTP handshake request")?;
            link.subscribe_c2().await.context("subscribing to C2")?;
            let response = link
                .next_c2()
                .await
                .context("awaiting BTP handshake response")?;
            BtpCentral::from_handshake_response(&response, Instant::now())
        };
        let core = match with_timeout(handshake_timeout, handshake).await {
            Ok(core) => core,
            Err(error) => {
                let _ = link.disconnect().await;
                return Err(error.context("BTP handshake"));
            }
        };
        log::info!(
            "BTP session with {address}: {}-byte segments, window {}",
            core.segment_size(),
            core.window()
        );
        Ok(Self {
            link,
            address,
            state: Mutex::new(State { core, closed: None }),
            tx_wake: Notify::new(),
            rx_wake: Notify::new(),
            space_wake: Notify::new(),
        })
    }

    /// The rs-matter address to use for this peer: `Address::Btp(self.address())`.
    pub fn address(&self) -> BtAddr {
        self.address
    }

    /// Pumps GATT <-> BTP. Must be polled concurrently with Matter traffic.
    /// Returns an error when the link drops or the peer violates BTP.
    pub async fn run(&self) -> Result<()> {
        let result = futures_lite::future::or(self.rx_loop(), self.tx_loop()).await;
        let error = match result {
            Ok(()) => anyhow!("BLE transport closed"),
            Err(e) => e,
        };
        self.close_with(format!("{error:#}"));
        Err(error)
    }

    /// Disconnects; pending and later sends fail.
    pub async fn close(&self) {
        self.close_with("closed".into());
        if let Err(error) = self.link.disconnect().await {
            log::debug!("BLE disconnect: {error:#}");
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn close_with(&self, reason: String) {
        self.state().closed.get_or_insert(reason);
        self.tx_wake.notify_waiters();
        self.rx_wake.notify_waiters();
        self.space_wake.notify_waiters();
    }

    fn check_open(state: &State) -> Result<()> {
        match &state.closed {
            Some(reason) => bail!("BLE transport closed: {reason}"),
            None => Ok(()),
        }
    }

    async fn rx_loop(&self) -> Result<()> {
        loop {
            let data = self.link.next_c2().await?;
            {
                let mut state = self.state();
                Self::check_open(&state)?;
                state.core.on_indication(&data, Instant::now())?;
            }
            self.rx_wake.notify_waiters();
            self.tx_wake.notify_waiters();
        }
    }

    async fn tx_loop(&self) -> Result<()> {
        loop {
            let mut woken = pin!(self.tx_wake.notified());
            woken.as_mut().enable();
            let deadline = loop {
                let (packet, deadline) = {
                    let mut state = self.state();
                    Self::check_open(&state)?;
                    let now = Instant::now();
                    state.core.check_timeouts(now)?;
                    (state.core.poll_transmit(now), state.core.next_deadline())
                };
                match packet {
                    Some(packet) => {
                        self.link.write_c1(&packet).await.context("writing C1")?;
                        self.space_wake.notify_waiters();
                    }
                    None => break deadline,
                }
            };
            match deadline {
                Some(at) => {
                    futures_lite::future::or(woken, async {
                        async_io::Timer::at(at).await;
                    })
                    .await
                }
                None => woken.await,
            }
        }
    }

    async fn wait_until<T>(
        &self,
        notify: &Notify,
        mut check: impl FnMut(&mut State) -> Option<T>,
    ) -> T {
        loop {
            let mut woken = pin!(notify.notified());
            woken.as_mut().enable();
            if let Some(value) = check(&mut self.state()) {
                return value;
            }
            woken.await;
        }
    }

    async fn send_sdu(&self, data: &[u8]) -> Result<(), Error> {
        if data.is_empty() || data.len() > btp::MAX_SDU {
            return Err(ErrorCode::InvalidArgument.into());
        }
        self.wait_until(&self.space_wake, |state| {
            if state.closed.is_some() {
                Some(Err::<(), Error>(ErrorCode::NoNetworkInterface.into()))
            } else if state.core.can_queue() {
                Some(state.core.queue_sdu(data).map_err(|e| {
                    log::warn!("BTP send: {e:#}");
                    ErrorCode::InvalidArgument.into()
                }))
            } else {
                None
            }
        })
        .await?;
        self.tx_wake.notify_waiters();
        Ok(())
    }

    async fn wait_sdu(&self) {
        self.wait_until(&self.rx_wake, |state| {
            state.core.sdu_available().then_some(())
        })
        .await
    }

    async fn recv_sdu(&self, buffer: &mut [u8]) -> usize {
        let sdu = self
            .wait_until(&self.rx_wake, |state| state.core.poll_sdu())
            .await;
        if sdu.len() > buffer.len() {
            log::warn!("truncating {}-byte BTP message", sdu.len());
        }
        let len = sdu.len().min(buffer.len());
        buffer[..len].copy_from_slice(&sdu[..len]);
        len
    }
}

impl NetworkSend for &BleTransport {
    async fn send_to(&mut self, data: &[u8], addr: Address) -> Result<(), Error> {
        if addr != Address::Btp(self.address) {
            return Err(ErrorCode::NoNetworkInterface.into());
        }
        self.send_sdu(data).await
    }
}

impl NetworkReceive for &BleTransport {
    async fn wait_available(&mut self) -> Result<(), Error> {
        self.wait_sdu().await;
        Ok(())
    }

    async fn recv_from(&mut self, buffer: &mut [u8]) -> Result<(usize, Address), Error> {
        let len = self.recv_sdu(buffer).await;
        Ok((len, Address::Btp(self.address)))
    }
}

async fn with_timeout<T>(limit: Duration, fut: impl Future<Output = Result<T>>) -> Result<T> {
    futures_lite::future::or(fut, async {
        async_io::Timer::after(limit).await;
        Err(anyhow!("timed out after {}s", limit.as_secs()))
    })
    .await
}

// ------------------------------------------------------------------ btleplug

type Events = Pin<Box<dyn Stream<Item = CentralEvent> + Send>>;
type Notifications = Pin<Box<dyn Stream<Item = ValueNotification> + Send>>;

struct BtleplugLink {
    peripheral: Peripheral,
    id: PeripheralId,
    c1: Characteristic,
    c2: Characteristic,
    notifications: tokio::sync::Mutex<Notifications>,
    events: tokio::sync::Mutex<Events>,
}

impl BtleplugLink {
    async fn connect(discriminator: Discriminator, timeout: Duration) -> Result<(Self, BtAddr)> {
        let manager = Manager::new().await.context("opening Bluetooth")?;
        let adapter = manager
            .adapters()
            .await
            .context("listing Bluetooth adapters")?
            .into_iter()
            .next()
            .context("no Bluetooth adapter")?;
        let mut events = adapter.events().await.context("Bluetooth events")?;
        for _ in 0..50 {
            if adapter.adapter_state().await.ok() == Some(CentralState::PoweredOn) {
                break;
            }
            async_io::Timer::after(Duration::from_millis(100)).await;
        }
        // No service filter: Matter devices advertise 0xFFF6 only as service
        // data, which not every backend's service filter matches.
        adapter
            .start_scan(ScanFilter::default())
            .await
            .context("starting Bluetooth scan")?;
        let service = Uuid::from_u128(MATTER_BLE_SERVICE_UUID);

        let scan = async {
            let matching = |data: Option<&Vec<u8>>| {
                data.and_then(|d| parse_service_data(d))
                    .is_some_and(|adv| discriminator.matches(adv.discriminator))
            };
            for peripheral in adapter.peripherals().await.unwrap_or_default() {
                if let Ok(Some(props)) = peripheral.properties().await {
                    if matching(props.service_data.get(&service)) {
                        return Ok(peripheral);
                    }
                }
            }
            while let Some(event) = events.next().await {
                let id = match event {
                    CentralEvent::ServiceDataAdvertisement { id, service_data } => {
                        if matching(service_data.get(&service)) {
                            return adapter
                                .peripheral(&id)
                                .await
                                .context("found device vanished");
                        }
                        continue;
                    }
                    CentralEvent::DeviceDiscovered(id) | CentralEvent::DeviceUpdated(id) => id,
                    _ => continue,
                };
                let Ok(peripheral) = adapter.peripheral(&id).await else {
                    continue;
                };
                if let Ok(Some(props)) = peripheral.properties().await {
                    if matching(props.service_data.get(&service)) {
                        return Ok(peripheral);
                    }
                }
            }
            bail!("Bluetooth event stream ended")
        };
        let found = with_timeout(timeout, scan).await;
        let _ = adapter.stop_scan().await;
        let peripheral = found
            .with_context(|| format!("no commissionable Matter device with {discriminator:?}"))?;
        let id = peripheral.id();
        log::info!("found Matter device {id}; connecting");

        with_timeout(CONNECT_TIMEOUT, async {
            peripheral.connect().await.context("connecting")?;
            peripheral
                .discover_services()
                .await
                .context("discovering GATT services")
        })
        .await?;
        let characteristic = |uuid: u128| {
            peripheral
                .characteristics()
                .into_iter()
                .find(|c| c.uuid == Uuid::from_u128(uuid) && c.service_uuid == service)
        };
        let (Some(c1), Some(c2)) = (
            characteristic(C1_CHARACTERISTIC_UUID),
            characteristic(C2_CHARACTERISTIC_UUID),
        ) else {
            let _ = peripheral.disconnect().await;
            bail!("device lacks the Matter C1/C2 characteristics");
        };
        // Subscribe to the stream before C2 so the handshake response can't be missed.
        let notifications = peripheral
            .notifications()
            .await
            .context("GATT notifications")?;

        let raw = peripheral.address().into_inner();
        let address = if raw != [0; 6] {
            BtAddr(raw)
        } else {
            // CoreBluetooth hides the MAC; derive a stable address from its UUID.
            let digest = Sha256::digest(id.to_string().as_bytes());
            let mut bytes = [0u8; 6];
            bytes.copy_from_slice(&digest[..6]);
            BtAddr(bytes)
        };
        Ok((
            Self {
                peripheral,
                id,
                c1,
                c2,
                notifications: tokio::sync::Mutex::new(notifications),
                events: tokio::sync::Mutex::new(events),
            },
            address,
        ))
    }
}

impl GattLink for BtleplugLink {
    fn write_c1<'a>(&'a self, data: &'a [u8]) -> LinkFuture<'a, ()> {
        Box::pin(async move {
            self.peripheral
                .write(&self.c1, data, WriteType::WithResponse)
                .await
                .context("C1 write")
        })
    }

    fn subscribe_c2(&self) -> LinkFuture<'_, ()> {
        Box::pin(async move {
            self.peripheral
                .subscribe(&self.c2)
                .await
                .context("C2 subscribe")
        })
    }

    fn next_c2(&self) -> LinkFuture<'_, Vec<u8>> {
        Box::pin(futures_lite::future::or(
            async move {
                loop {
                    let notification = self.notifications.lock().await.next().await;
                    match notification {
                        Some(n) if n.uuid == self.c2.uuid => return Ok(n.value),
                        Some(_) => continue,
                        None => bail!("Bluetooth connection closed"),
                    }
                }
            },
            async move {
                loop {
                    let event = self.events.lock().await.next().await;
                    match event {
                        Some(CentralEvent::DeviceDisconnected(id)) if id == self.id => {
                            bail!("Bluetooth device disconnected")
                        }
                        Some(_) => continue,
                        None => core::future::pending::<()>().await,
                    }
                }
            },
        ))
    }

    fn disconnect(&self) -> LinkFuture<'_, ()> {
        Box::pin(async move { self.peripheral.disconnect().await.context("disconnecting") })
    }
}
