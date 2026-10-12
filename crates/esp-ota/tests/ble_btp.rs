//! The BTP central against rs-matter's real BTP peripheral, with GATT
//! simulated in-process: C1 writes go to `Btp::process_incoming`, and
//! `Btp::process_outgoing` output comes back as C2 indications. Finishes
//! with a full commissioning (PASE, attestation, CSR, certificates over BLE;
//! CASE and CommissioningComplete over UDP) between an rs-matter device and
//! the esp-ota controller.

use core::pin::pin;
use std::cell::Cell;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use embassy_futures::select::{select, select3, select4, Either, Either4};
use futures_lite::future::{block_on, poll_once};
use rs_matter::cert::gen::VALID_FOREVER;
use rs_matter::cert::{MAX_CERT_TLV_AND_ASN1_LEN, MAX_CERT_TLV_LEN};
use rs_matter::crypto::{test_only_crypto, Crypto};
use rs_matter::dm::clusters::net_comm::DummyNetworks;
use rs_matter::dm::devices::test::{TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter::dm::networks::unix::UnixNetifs;
use rs_matter::dm::{endpoints, AttrChangeNotifier, DataModel, Node};
use rs_matter::im::subscriptions::DEFAULT_MAX_SUBSCRIPTIONS;
use rs_matter::im::{InteractionModel, InteractionModelState};
use rs_matter::onboard::noc::NocGenerator;
use rs_matter::onboard::{CommissionOptions, Commissioner};
use rs_matter::persist::DummyKvBlobStore;
use rs_matter::respond::DefaultResponder;
use rs_matter::sc::pase::MAX_COMM_WINDOW_TIMEOUT_SECS;
use rs_matter::transport::exchange::MatterBuffers;
use rs_matter::transport::network::btp::{AdvData, Btp};
use rs_matter::transport::network::{Address, BtAddr, ChainedNetwork, NoNetwork};
use rs_matter::utils::init::InitMaybeUninit;
use rs_matter::utils::select::Coalesce;
use rs_matter::{root_endpoint, Matter};
use tokio::sync::{mpsc, Mutex, Notify};

use esp_ota::ble::btp::{self, BtpCentral};
use esp_ota::ble::{parse_service_data, BleTransport, Discriminator, GattLink, LinkFuture};
use esp_ota::controller::{new_fabric, peer, secret_key, udp_socket, Controller};
use esp_ota::store::Store;

const CENTRAL: BtAddr = BtAddr([0xC0, 1, 2, 3, 4, 5]);
const PERIPHERAL: BtAddr = BtAddr([0xD0, 1, 2, 3, 4, 5]);
/// What a macOS central typically negotiates; rs-matter's peripheral sees it
/// differ from the announced 23 and falls back to 20-byte segments.
const DEVICE_ATT_MTU: Option<u16> = Some(185);

// ------------------------------------------------------------- advertisement

#[test]
fn advertisement_matches_rs_matter_layout() {
    for discriminator in [0u16, 3840, 0x0abc, 0x0fff] {
        let data: Vec<u8> = AdvData::new(&TEST_DEV_DET, discriminator)
            .service_payload_iter()
            .collect();
        let adv = parse_service_data(&data).unwrap();
        assert_eq!(adv.discriminator, discriminator);
        assert_eq!(adv.version, 0);
        assert_eq!(adv.vendor_id, Some(TEST_DEV_DET.vid));
        assert_eq!(adv.product_id, Some(TEST_DEV_DET.pid));
        assert!(Discriminator::Long(discriminator).matches(adv.discriminator));
        assert!(Discriminator::Short((discriminator >> 8) as u8).matches(adv.discriminator));
        assert!(!Discriminator::Long(discriminator ^ 1).matches(adv.discriminator));
        assert!(
            !Discriminator::Short(((discriminator >> 8) as u8 + 1) & 0xF)
                .matches(adv.discriminator)
        );
    }
    // Advertisement version bits and a non-commissionable opcode.
    let adv = parse_service_data(&[0x00, 0x00, 0x1F, 0xF1, 0xFF]).unwrap();
    assert_eq!((adv.discriminator, adv.version), (0xF00, 1));
    assert_eq!(adv.vendor_id, Some(0xFFF1));
    assert_eq!(adv.product_id, None);
    assert!(parse_service_data(&[0x01, 0x00, 0x0F]).is_none());
    assert!(parse_service_data(&[0x00, 0x00]).is_none());
}

// ------------------------------------------------- synchronous protocol pump

fn peripheral() -> &'static Btp {
    Box::leak(Box::new(Btp::new()))
}

fn handshake(btp: &Btp, gatt_mtu: Option<u16>) -> BtpCentral {
    btp.process_incoming(gatt_mtu, CENTRAL, &btp::handshake_request())
        .unwrap();
    let mut buf = [0u8; 512];
    let len = btp.process_outgoing(gatt_mtu, &mut buf).unwrap();
    BtpCentral::from_handshake_response(&buf[..len], Instant::now()).unwrap()
}

struct Pair {
    btp: &'static Btp,
    central: BtpCentral,
    clock: Instant,
    at_peripheral: Vec<Vec<u8>>,
    at_central: Vec<Vec<u8>>,
    c1_writes: usize,
}

impl Pair {
    fn new() -> Self {
        let btp = peripheral();
        let central = handshake(btp, DEVICE_ATT_MTU);
        Self {
            btp,
            central,
            clock: Instant::now(),
            at_peripheral: Vec::new(),
            at_central: Vec::new(),
            c1_writes: 0,
        }
    }

    fn step(&mut self) -> bool {
        let mut progress = false;
        while let Some(segment) = self.central.poll_transmit(self.clock) {
            assert!(segment.len() <= 20, "segment of {} bytes", segment.len());
            self.btp
                .process_incoming(DEVICE_ATT_MTU, CENTRAL, &segment)
                .unwrap();
            self.c1_writes += 1;
            progress = true;
        }
        let mut buf = vec![0u8; 4096];
        while let Some(result) = block_on(poll_once(self.btp.recv(&mut buf))) {
            let (len, addr) = result.unwrap();
            assert_eq!(addr, CENTRAL);
            self.at_peripheral.push(buf[..len].to_vec());
            progress = true;
        }
        loop {
            let len = self.btp.process_outgoing(DEVICE_ATT_MTU, &mut buf).unwrap();
            if len == 0 {
                break;
            }
            assert!(len <= 20);
            self.central.on_indication(&buf[..len], self.clock).unwrap();
            progress = true;
        }
        while let Some(sdu) = self.central.poll_sdu() {
            self.at_central.push(sdu);
            progress = true;
        }
        self.central.check_timeouts(self.clock).unwrap();
        progress
    }

    /// Runs until neither side has anything left, letting the central's
    /// standalone-ACK delay elapse when otherwise idle.
    fn settle(&mut self) {
        for _ in 0..100_000 {
            if !self.step() {
                self.clock += Duration::from_secs(1);
                if !self.step() {
                    return;
                }
            }
        }
        panic!("BTP pump never settled");
    }

    fn peripheral_send(&self, data: &[u8]) {
        block_on(poll_once(self.btp.send(data, CENTRAL)))
            .expect("peripheral send buffer busy")
            .unwrap();
    }
}

fn payload(len: usize, seed: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + seed * 13) as u8).collect()
}

#[test]
fn handshake_negotiates_minimum_segments() {
    for gatt_mtu in [None, Some(23), Some(185), Some(247)] {
        let central = handshake(peripheral(), gatt_mtu);
        assert_eq!(central.segment_size(), 20, "GATT MTU {gatt_mtu:?}");
        assert_eq!(central.window(), btp::REQUESTED_WINDOW);
    }
    assert_eq!(
        btp::handshake_request(),
        [0x65, 0x6c, 0x04, 0, 0, 0, 23, 0, btp::REQUESTED_WINDOW]
    );
}

#[test]
fn large_message_central_to_peripheral() {
    let mut pair = Pair::new();
    let sdu = payload(1200, 1);
    pair.central.queue_sdu(&sdu).unwrap();
    pair.settle();
    assert_eq!(pair.at_peripheral, vec![sdu]);
    // 1200 bytes in 20-byte segments, plus a few standalone ACKs.
    assert!(pair.c1_writes >= 1200 / 18, "{} writes", pair.c1_writes);
}

#[test]
fn large_message_peripheral_to_central() {
    let mut pair = Pair::new();
    let sdu = payload(1232, 2);
    pair.peripheral_send(&sdu);
    pair.settle();
    assert_eq!(pair.at_central, vec![sdu]);
}

#[test]
fn many_messages_both_ways_wrap_sequence_numbers() {
    let mut pair = Pair::new();
    let mut sent_c = Vec::new();
    let mut sent_p = Vec::new();
    for i in 0..120 {
        // Matter messages are at least ~30 bytes; vary across segment counts.
        let c = payload(21 + (i * 97) % 1212, i);
        let p = payload(21 + (i * 61 + 500) % 1212, i + 1000);
        pair.central.queue_sdu(&c).unwrap();
        if i % 3 != 2 {
            pair.central.queue_sdu(&c[..(c.len() / 2).max(21)]).unwrap();
            sent_c.push(c.clone());
            sent_c.push(c[..(c.len() / 2).max(21)].to_vec());
        } else {
            sent_c.push(c);
        }
        pair.peripheral_send(&p);
        sent_p.push(p);
        pair.settle();
    }
    assert_eq!(pair.at_peripheral, sent_c);
    assert_eq!(pair.at_central, sent_p);
    assert!(pair.c1_writes > 3 * 256, "sequence numbers wrapped");
    assert!(pair.central.tx_idle());
}

#[test]
fn rejects_out_of_sequence_and_unsent_acks() {
    let mut central = handshake(peripheral(), None);
    let now = Instant::now();
    // Peer's next sequence is 1; 2 is a gap.
    assert!(central
        .on_indication(&[0x05, 2, 3, 0, 1, 2, 3], now)
        .is_err());
    let mut central = handshake(peripheral(), None);
    // ACK for our sequence 0, which we never sent.
    assert!(central.on_indication(&[0x08, 0, 1], now).is_err());
    let mut central = handshake(peripheral(), None);
    assert!(central
        .on_indication(&[0x05, 1, 3, 0, 1, 2, 3], now)
        .is_ok());
    assert_eq!(central.poll_sdu(), Some(vec![1, 2, 3]));
}

#[test]
fn unacknowledged_segments_time_out() {
    let btp = peripheral();
    let mut central = handshake(btp, None);
    let start = Instant::now();
    central.queue_sdu(&payload(100, 3)).unwrap();
    while central.poll_transmit(start).is_some() {}
    assert!(central
        .check_timeouts(start + Duration::from_secs(5))
        .is_ok());
    assert!(central
        .check_timeouts(start + btp::ACK_RECEIVE_TIMEOUT + Duration::from_secs(1))
        .is_err());
}

// -------------------------------------------------- simulated GATT, async

struct SimLink {
    btp: &'static Btp,
    subscribed: &'static (Cell<bool>, Notify),
    indications: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
}

impl GattLink for SimLink {
    fn write_c1<'a>(&'a self, data: &'a [u8]) -> LinkFuture<'a, ()> {
        Box::pin(async move {
            self.btp
                .process_incoming(DEVICE_ATT_MTU, CENTRAL, data)
                .map_err(|e| anyhow!("peripheral rejected C1 write: {e:?}"))
        })
    }

    fn subscribe_c2(&self) -> LinkFuture<'_, ()> {
        Box::pin(async move {
            self.subscribed.0.set(true);
            self.subscribed.1.notify_waiters();
            Ok(())
        })
    }

    fn next_c2(&self) -> LinkFuture<'_, Vec<u8>> {
        Box::pin(async move {
            self.indications
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| anyhow!("link closed"))
        })
    }

    fn disconnect(&self) -> LinkFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// The peripheral's GATT side, as rs-matter-embassy does it: indicate
/// `process_outgoing` output once C2 is subscribed.
async fn indicate(
    btp: &Btp,
    subscribed: &(Cell<bool>, Notify),
    tx: mpsc::UnboundedSender<Vec<u8>>,
) -> Result<()> {
    while !subscribed.0.get() {
        subscribed.1.notified().await;
    }
    let mut buf = [0u8; 512];
    loop {
        let len = btp
            .process_outgoing(DEVICE_ATT_MTU, &mut buf)
            .map_err(|e| anyhow!("{e:?}"))?;
        if len > 0 {
            tx.send(buf[..len].to_vec())?;
            // A real indication takes a connection event to confirm.
            async_io::Timer::after(Duration::from_millis(1)).await;
        } else {
            btp.wait_outgoing().await;
        }
    }
}

fn sim_link(btp: &'static Btp) -> (SimLink, impl core::future::Future<Output = Result<()>>) {
    let subscribed: &'static (Cell<bool>, Notify) =
        Box::leak(Box::new((Cell::new(false), Notify::new())));
    let (tx, rx) = mpsc::unbounded_channel();
    let link = SimLink {
        btp,
        subscribed,
        indications: Mutex::new(rx),
    };
    (link, indicate(btp, subscribed, tx))
}

#[test]
fn transport_round_trips_messages() {
    block_on(async {
        let btp = peripheral();
        let (link, gatt) = sim_link(btp);
        let mut gatt = pin!(gatt);
        let opened = select(
            &mut gatt,
            BleTransport::with_link(Box::new(link), PERIPHERAL, Duration::from_secs(5)),
        )
        .await;
        let Either::Second(ble) = opened else {
            panic!("peripheral GATT stopped")
        };
        let ble = ble.unwrap();
        assert_eq!(ble.address(), PERIPHERAL);

        let echo = async {
            let mut buf = vec![0u8; 2048];
            loop {
                let (len, addr) = btp.recv(&mut buf).await.unwrap();
                assert_eq!(addr, CENTRAL);
                let mut reply = buf[..len].to_vec();
                reply.reverse();
                btp.send(&reply, CENTRAL).await.unwrap();
            }
        };
        let client = async {
            use rs_matter::transport::network::{NetworkReceive, NetworkSend};
            let (mut tx, mut rx) = (&ble, &ble);
            let mut buf = vec![0u8; 2048];
            for i in 0..40 {
                let msg = payload(30 + i * 29, i);
                tx.send_to(&msg, Address::Btp(PERIPHERAL)).await.unwrap();
                rx.wait_available().await.unwrap();
                let (len, addr) = rx.recv_from(&mut buf).await.unwrap();
                assert_eq!(addr, Address::Btp(PERIPHERAL));
                let mut expected = msg;
                expected.reverse();
                assert_eq!(&buf[..len], &expected[..]);
            }
            assert!(tx
                .send_to(&[1; 40], Address::Udp("[::1]:5540".parse().unwrap()))
                .await
                .is_err());
        };
        match select4(&mut gatt, ble.run(), echo, client).await {
            Either4::Fourth(()) => {}
            Either4::First(r) => panic!("peripheral GATT stopped: {r:?}"),
            Either4::Second(r) => panic!("BLE transport stopped: {r:?}"),
            Either4::Third(()) => unreachable!(),
        }
        ble.close().await;
        use rs_matter::transport::network::NetworkSend;
        assert!((&ble)
            .send_to(&[1; 40], Address::Btp(PERIPHERAL))
            .await
            .is_err());
    });
}

// ----------------------------------------------- full commissioning over BLE

const PASSCODE: u32 = 20202021;

const NODE: Node<'static> = Node {
    endpoints: &[root_endpoint!(eth)],
};

fn data_model(rand: impl rs_matter::crypto::RngCore + Copy) -> impl DataModel {
    (
        NODE,
        endpoints::EthSysHandlerBuilder::new()
            .netif_diag(&UnixNetifs)
            .build(rand),
    )
}

struct NoNotify;

impl AttrChangeNotifier for NoNotify {
    fn notify_attr_changed(&self, _: u16, _: u32, _: u32) {}
    fn notify_cluster_changed(&self, _: u16, _: u32) {}
    fn notify_endpoint_changed(&self, _: u16) {}
    fn notify_all_changed(&self) {}
}

type DeviceState = InteractionModelState<DummyNetworks, DEFAULT_MAX_SUBSCRIPTIONS, 0>;

#[test]
fn commissions_over_simulated_ble() {
    let _ = env_logger::try_init_from_env(env_logger::Env::default().filter_or("RUST_LOG", "warn"));
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| block_on(commission_over_ble()))
        .unwrap()
        .join()
        .unwrap();
}

async fn commission_over_ble() {
    // Device: rs-matter on BTP (peripheral) plus UDP for CASE.
    let device_matter: &'static Matter = Box::leak(Box::new_uninit()).init_with(Matter::init(
        &TEST_DEV_DET,
        TEST_DEV_COMM,
        &TEST_DEV_ATT,
        0,
    ));
    let device_crypto = test_only_crypto();
    let rand = device_crypto.rand().unwrap();
    let buffers: &'static MatterBuffers =
        Box::leak(Box::new_uninit()).init_with(MatterBuffers::init());
    let state: &'static DeviceState =
        Box::leak(Box::new(InteractionModelState::new(DummyNetworks)));
    let kv = device_matter.kv(DummyKvBlobStore);
    let dm = InteractionModel::new(
        device_matter,
        &device_crypto,
        buffers,
        data_model(rand),
        &kv,
        state,
    );
    device_matter
        .open_basic_comm_window(MAX_COMM_WINDOW_TIMEOUT_SECS, &device_crypto, &NoNotify)
        .unwrap();
    let responder = DefaultResponder::new(&dm);
    let device_socket = async_io::Async::<UdpSocket>::bind(SocketAddr::V6(SocketAddrV6::new(
        Ipv6Addr::LOCALHOST,
        0,
        0,
        0,
    )))
    .unwrap();
    let device_addr = device_socket.get_ref().local_addr().unwrap();
    let btp = peripheral();
    let is_btp = |addr: &Address| addr.is_btp();
    let device_send = ChainedNetwork::new(is_btp, btp, &device_socket);
    let device_recv = ChainedNetwork::new(is_btp, btp, &device_socket);
    let (link, gatt) = sim_link(btp);
    let device = async {
        select4(
            device_matter.run(&device_crypto, device_send, device_recv, NoNetwork),
            responder.run::<4, 4>(),
            dm.run(),
            async { gatt.await.map_err(|e| panic!("peripheral GATT: {e:#}")) },
        )
        .coalesce()
        .await
    };

    // Host: the esp-ota controller on BLE + UDP.
    let host_crypto = esp_ota::controller::crypto();
    let mut store = Store {
        fabric: new_fabric(&host_crypto).unwrap(),
        devices: Default::default(),
    };
    let node_id = store.allocate_node_id();
    let ctl = Controller::new(host_crypto, &store).unwrap();
    let host_socket = udp_socket().unwrap();
    let host = async {
        let ble =
            BleTransport::with_link(Box::new(link), PERIPHERAL, Duration::from_secs(5)).await?;
        let is_btp = |addr: &Address| matches!(addr, Address::Btp(_));
        let send = ChainedNetwork::new(is_btp, &ble, &host_socket);
        let recv = ChainedNetwork::new(is_btp, &ble, &host_socket);
        let task = ctl.run(send, recv, async {
            let icac_key = secret_key(&ctl.fabric.icac_key)?;
            let mut noc_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
            let mut nocs =
                NocGenerator::new(ctl.matter, icac_key.reference(), ctl.fab_idx, &mut noc_buf)
                    .map_err(|e| anyhow!("{e:?}"))?;
            let mut buf = [0u8; MAX_CERT_TLV_LEN];
            let mut commissioner =
                Commissioner::new(ctl.matter, &ctl.crypto, ctl.fab_idx, &mut nocs, &mut buf);
            let opts = CommissionOptions {
                allow_test_attestation: true,
                fail_safe_secs: 120,
            };
            let result = commissioner
                .commission(
                    Address::Btp(ble.address()),
                    PASSCODE,
                    &opts,
                    node_id,
                    VALID_FOREVER,
                )
                .await
                .map_err(|e| anyhow!("commissioning over BLE: {e:?}"))?;
            commissioner
                .complete_via_case(Address::Udp(peer(device_addr)), &result)
                .await
                .map_err(|e| anyhow!("CASE over UDP: {e:?}"))?;
            Ok(())
        });
        match select(task, ble.run()).await {
            Either::First(result) => result,
            Either::Second(result) => Err(anyhow!("BLE transport stopped: {result:?}")),
        }
    };

    let mut device = pin!(device);
    let mut host = pin!(host);
    let timeout = async {
        async_io::Timer::after(Duration::from_secs(120)).await;
    };
    match select3(&mut device, &mut host, timeout).await {
        embassy_futures::select::Either3::First(result) => panic!("device stopped: {result:?}"),
        embassy_futures::select::Either3::Second(result) => result.unwrap(),
        embassy_futures::select::Either3::Third(()) => panic!("commissioning timed out"),
    }
    let fabrics = device_matter
        .with_state(|state| Ok::<_, rs_matter::error::Error>(state.fabrics.iter().count()))
        .unwrap();
    assert_eq!(fabrics, 1);
    let _ = store;
}
