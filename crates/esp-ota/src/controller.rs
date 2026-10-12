//! The host's rs-matter node: creates or loads the private fabric and runs
//! the transport while a task uses it.

use core::num::NonZeroU8;
use core::pin::pin;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};

use anyhow::{anyhow, Context, Result};
use embassy_futures::select::{select, Either};
use rs_matter::cert::gen::VALID_FOREVER;
use rs_matter::cert::MAX_CERT_TLV_AND_ASN1_LEN;
use rs_matter::crypto::{
    default_crypto, CanonAeadKey, CanonPkcSecretKey, Crypto, RngCore as _, SecretKey,
    SigningSecretKey,
};
use rs_matter::dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter::onboard::cac::{IcacGenerator, RcacGenerator};
use rs_matter::onboard::noc::NocGenerator;
use rs_matter::transport::network::{NetworkReceive, NetworkSend, NoNetwork};
use rs_matter::utils::init::InitMaybeUninit;
use rs_matter::Matter;

use crate::store::{hex_bytes, FabricRecord, Store};

/// Vendor ID recorded for our fabric (the test vendor; not certified).
pub const VENDOR_ID: u16 = 0xFFF1;
pub const CONTROLLER_NODE_ID: u64 = 0x0000_0000_E5B0_0001;
pub const FIRST_DEVICE_NODE_ID: u64 = 0x0000_0000_E5B0_1000;

pub fn crypto() -> impl Crypto {
    default_crypto(rand::rngs::OsRng, DAC_PRIVKEY)
}

/// A new fabric: root CA (discarded after signing), intermediate CA, and the
/// controller's own operational identity.
pub fn new_fabric<C: Crypto>(crypto: &C) -> Result<FabricRecord> {
    let mut fabric_id_bytes = [0u8; 8];
    crypto.rand()?.fill_bytes(&mut fabric_id_bytes);
    let fabric_id = u64::from_be_bytes(fabric_id_bytes) | 1;

    let mut rcac_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut rcac_gen = RcacGenerator::new(&mut rcac_buf);
    let (rcac_key, rcac) = rcac_gen.generate(crypto, fabric_id, VALID_FOREVER)?;

    let mut icac_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut icac_gen = IcacGenerator::new(&mut icac_buf);
    let (icac_key, icac) = icac_gen.generate(crypto, rcac_key.reference(), rcac, VALID_FOREVER)?;
    drop(rcac_key);

    let controller_key = crypto.generate_secret_key()?;
    let mut csr_buf = [0u8; 256];
    let csr = controller_key.csr(&mut csr_buf)?;
    let mut controller_key_canon = CanonPkcSecretKey::new();
    controller_key.write_canon(&mut controller_key_canon)?;

    let mut noc_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut nocs = NocGenerator::create(icac_key.reference(), rcac, icac, &mut noc_buf)?;
    let noc = nocs.generate(crypto, csr, CONTROLLER_NODE_ID, &[], VALID_FOREVER)?;

    let mut ipk = CanonAeadKey::new();
    crypto.rand()?.fill_bytes(ipk.access_mut());

    Ok(FabricRecord {
        fabric_id,
        vendor_id: VENDOR_ID,
        controller_node_id: CONTROLLER_NODE_ID,
        rcac: hex::encode(rcac),
        icac: hex::encode(icac),
        controller_noc: hex::encode(noc),
        icac_key: hex::encode(icac_key.access()),
        controller_key: hex::encode(controller_key_canon.access()),
        ipk: hex::encode(ipk.access()),
        next_node_id: FIRST_DEVICE_NODE_ID,
    })
}

pub fn secret_key(hex_text: &str) -> Result<CanonPkcSecretKey> {
    let bytes = hex_bytes(hex_text)?;
    let mut key = CanonPkcSecretKey::new();
    if bytes.len() != key.access().len() {
        return Err(anyhow!("stored key has the wrong length"));
    }
    key.access_mut().copy_from_slice(&bytes);
    Ok(key)
}

/// A running host node with our fabric installed.
pub struct Controller<'a, C> {
    pub matter: &'a Matter<'a>,
    pub crypto: C,
    pub fab_idx: NonZeroU8,
    pub compressed_fabric_id: u64,
    pub fabric: FabricRecord,
}

impl<C: Crypto> Controller<'static, C> {
    pub fn new(crypto: C, store: &Store) -> Result<Self> {
        let matter: &'static mut core::mem::MaybeUninit<Matter<'static>> =
            Box::leak(Box::new_uninit());
        let matter: &'static Matter<'static> =
            matter.init_with(Matter::init(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, 0));
        let fabric = store.fabric.clone();
        let rcac = hex_bytes(&fabric.rcac)?;
        let icac = hex_bytes(&fabric.icac)?;
        let noc = hex_bytes(&fabric.controller_noc)?;
        let key = secret_key(&fabric.controller_key)?;
        let ipk_bytes = hex_bytes(&fabric.ipk)?;
        let mut ipk = CanonAeadKey::new();
        if ipk_bytes.len() != ipk.access().len() {
            return Err(anyhow!("stored IPK has the wrong length"));
        }
        ipk.access_mut().copy_from_slice(&ipk_bytes);
        let (fab_idx, compressed_fabric_id) = matter.with_state(|state| {
            state
                .fabrics
                .add(
                    &crypto,
                    key.reference(),
                    &rcac,
                    &noc,
                    &icac,
                    Some(ipk.reference()),
                    fabric.vendor_id,
                    fabric.controller_node_id,
                )
                .map(|f| (f.fab_idx(), f.compressed_fabric_id()))
        })?;
        Ok(Self {
            matter,
            crypto,
            fab_idx,
            compressed_fabric_id,
            fabric,
        })
    }
}

impl<'a, C: Crypto> Controller<'a, C> {
    /// Runs `task` while the transport pumps the network.
    pub async fn run<S, N, T, R>(&self, send: S, recv: N, task: T) -> Result<R>
    where
        S: NetworkSend,
        N: NetworkReceive,
        T: core::future::Future<Output = Result<R>>,
    {
        let mut transport = pin!(self.matter.run(&self.crypto, send, recv, NoNetwork));
        let mut task = pin!(task);
        match select(&mut transport, &mut task).await {
            Either::First(result) => Err(anyhow!("Matter transport stopped: {result:?}")),
            Either::Second(result) => result,
        }
    }
}

/// A dual-stack UDP socket on an ephemeral port. IPv4 peers are addressed
/// as IPv4-mapped IPv6 addresses.
pub fn udp_socket() -> Result<async_io::Async<UdpSocket>> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_only_v6(false)?;
    socket.bind(&SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0)).into())?;
    let socket: UdpSocket = socket.into();
    async_io::Async::new(socket).context("registering UDP socket")
}

/// Maps IPv4 to the IPv6-mapped form the dual-stack socket needs.
pub fn peer(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V4(v4) => {
            SocketAddr::V6(SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0))
        }
        v6 => v6,
    }
}
