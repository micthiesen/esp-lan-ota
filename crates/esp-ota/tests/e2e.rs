//! End to end over loopback UDP: an rs-matter device running the real
//! `OtaCluster` on RAM flash is enrolled by the real `esp-ota` commissioner,
//! then updated through the cluster. Proves the Matter wiring, ACLs, chunking,
//! resume and staging without an ESP.

use core::cell::RefCell;
use core::pin::pin;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};
use std::rc::Rc;

use embassy_futures::select::select3;
use embedded_storage::nor_flash::{
    ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
};
use esp_lan_ota::matter::{OtaCluster, OtaService, CLUSTER_ID};
use esp_lan_ota::otadata::{Entry, OtaData};
use esp_lan_ota::partition::Layout;
use esp_lan_ota::{Chip, Identity, ImageState, Name, Ota, Slot};
use rs_matter::crypto::{test_only_crypto, Crypto};
use rs_matter::dm::clusters::desc::{self, ClusterHandler as _};
use rs_matter::dm::clusters::net_comm::DummyNetworks;
use rs_matter::dm::devices::test::{TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter::dm::devices::DEV_TYPE_ON_OFF_LIGHT;
use rs_matter::dm::networks::unix::UnixNetifs;
use rs_matter::dm::{endpoints, Async, DataModel, Dataver, Endpoint, EpClMatcher, Node};
use rs_matter::im::subscriptions::DEFAULT_MAX_SUBSCRIPTIONS;
use rs_matter::im::{InteractionModel, InteractionModelState};
use rs_matter::persist::DummyKvBlobStore;
use rs_matter::respond::DefaultResponder;
use rs_matter::sc::pase::MAX_COMM_WINDOW_TIMEOUT_SECS;
use rs_matter::transport::exchange::MatterBuffers;
use rs_matter::transport::network::{Address, NoNetwork};
use rs_matter::utils::init::InitMaybeUninit;
use rs_matter::utils::select::Coalesce;
use rs_matter::{clusters, devices, root_endpoint, Matter};
use sha2::{Digest, Sha256};

use esp_ota::client::Client;
use esp_ota::commission::commission;
use esp_ota::controller::{new_fabric, udp_socket, Controller};
use esp_ota::store::Store;

const PASSCODE: u32 = 20202021;
const FLASH_SIZE: usize = 0x100000;
const SLOT_LEN: u32 = 0x60000;

// ---------------------------------------------------------------- RAM flash

#[derive(Clone)]
struct RamFlash(Rc<RefCell<Vec<u8>>>);

#[derive(Debug)]
struct RamError;

impl NorFlashError for RamError {
    fn kind(&self) -> NorFlashErrorKind {
        NorFlashErrorKind::Other
    }
}

impl ErrorType for RamFlash {
    type Error = RamError;
}

impl ReadNorFlash for RamFlash {
    const READ_SIZE: usize = 4;
    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), RamError> {
        let data = self.0.borrow();
        bytes.copy_from_slice(&data[offset as usize..offset as usize + bytes.len()]);
        Ok(())
    }
    fn capacity(&self) -> usize {
        FLASH_SIZE
    }
}

impl NorFlash for RamFlash {
    const WRITE_SIZE: usize = 4;
    const ERASE_SIZE: usize = 4096;
    fn erase(&mut self, from: u32, to: u32) -> Result<(), RamError> {
        self.0.borrow_mut()[from as usize..to as usize].fill(0xFF);
        Ok(())
    }
    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), RamError> {
        let mut data = self.0.borrow_mut();
        for (i, b) in bytes.iter().enumerate() {
            data[offset as usize + i] &= b;
        }
        Ok(())
    }
}

fn entry(kind: u8, subtype: u8, offset: u32, len: u32) -> [u8; 32] {
    let mut raw = [0u8; 32];
    raw[0..2].copy_from_slice(&[0xAA, 0x50]);
    raw[2] = kind;
    raw[3] = subtype;
    raw[4..8].copy_from_slice(&offset.to_le_bytes());
    raw[8..12].copy_from_slice(&len.to_le_bytes());
    raw
}

/// Flash as after `espflash flash --erase-parts otadata` and the bootloader's
/// first boot: ota_0 recorded by an unmarked bootloader record.
fn flashed_device() -> RamFlash {
    let mut data = vec![0xFFu8; FLASH_SIZE];
    let table = [
        entry(0x01, 0x02, 0x9000, 0x4000),
        entry(0x01, 0x00, 0xD000, 0x2000),
        entry(0x00, 0x10, 0x10000, SLOT_LEN),
        entry(0x00, 0x11, 0x10000 + SLOT_LEN, SLOT_LEN),
    ];
    for (i, raw) in table.iter().enumerate() {
        data[0x8000 + i * 32..0x8000 + i * 32 + 32].copy_from_slice(raw);
    }
    data[0xD000..0xD020].copy_from_slice(&Entry::unmarked(1, ImageState::Valid).encode());
    RamFlash(Rc::new(RefCell::new(data)))
}

fn image(project: &str, version: &str, len: usize) -> Vec<u8> {
    let mut image = vec![0u8; len];
    for (i, b) in image.iter_mut().enumerate() {
        *b = (i * 31 + 7) as u8;
    }
    image[0] = 0xE9;
    image[12..14].copy_from_slice(&Chip::Esp32c6.image_id().to_le_bytes());
    image[32..36].copy_from_slice(&0xABCD_5432u32.to_le_bytes());
    image[48..80].fill(0);
    image[48..48 + version.len()].copy_from_slice(version.as_bytes());
    image[80..112].fill(0);
    image[80..80 + project.len()].copy_from_slice(project.as_bytes());
    image
}

const IDENTITY: Identity = Identity {
    chip: Chip::Esp32c6,
    project: match Name::new("e2e-demo") {
        Some(n) => n,
        None => panic!(),
    },
    version: match Name::new("1.0.0") {
        Some(n) => n,
        None => panic!(),
    },
};

const NODE: Node<'static> = Node {
    endpoints: &[
        root_endpoint!(eth),
        Endpoint::new(
            1,
            devices!(DEV_TYPE_ON_OFF_LIGHT),
            clusters!(
                desc::DescHandler::CLUSTER,
                OtaCluster::<'static, RamFlash>::CLUSTER
            ),
        ),
    ],
};

fn data_model<'a>(
    mut rand: impl rs_matter::crypto::RngCore + Copy,
    service: &'a OtaService<RamFlash>,
) -> impl DataModel + 'a {
    (
        NODE,
        endpoints::EthSysHandlerBuilder::new()
            .netif_diag(&UnixNetifs)
            .build(rand)
            .chain(
                EpClMatcher::new(Some(1), Some(desc::DescHandler::CLUSTER.id)),
                Async(desc::DescHandler::new(Dataver::new_rand(&mut rand)).adapt()),
            )
            .chain(
                EpClMatcher::new(Some(1), Some(CLUSTER_ID)),
                Async(OtaCluster::new(service, Dataver::new_rand(&mut rand))),
            ),
    )
}

type DeviceState = InteractionModelState<DummyNetworks, DEFAULT_MAX_SUBSCRIPTIONS, 0>;

#[test]
fn enroll_then_update_over_matter() {
    let _ = env_logger::try_init_from_env(env_logger::Env::default().filter_or("RUST_LOG", "warn"));
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| futures_lite::future::block_on(run()))
        .unwrap()
        .join()
        .unwrap();
}

async fn run() {
    let flash = flashed_device();
    let booted = 0x10000;

    let ota = Ota::start(flash.clone(), booted, IDENTITY).unwrap();
    let boot = ota.boot_info();
    assert!(boot.enrollment_window, "first boot after USB flash");
    let service = OtaService::new(ota);

    // Device.
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
        data_model(rand, &service),
        &kv,
        state,
    );
    assert!(
        esp_lan_ota::matter::open_enrollment_window(&boot, device_matter, &device_crypto)
            .unwrap()
            .is_some()
    );
    let _ = MAX_COMM_WINDOW_TIMEOUT_SECS;
    let responder = DefaultResponder::new(&dm);
    let device_socket = async_io::Async::<UdpSocket>::bind(SocketAddr::V6(SocketAddrV6::new(
        Ipv6Addr::LOCALHOST,
        0,
        0,
        0,
    )))
    .unwrap();
    let device_addr = device_socket.get_ref().local_addr().unwrap();
    let device = async {
        select3(
            device_matter.run(&device_crypto, &device_socket, &device_socket, NoNetwork),
            responder.run::<4, 4>(),
            dm.run(),
        )
        .coalesce()
        .await
    };

    // Host.
    let host_crypto = esp_ota::controller::crypto();
    let mut store = Store {
        fabric: new_fabric(&host_crypto).unwrap(),
        devices: Default::default(),
    };
    let node_id = store.allocate_node_id();
    let ctl = Controller::new(host_crypto, &store).unwrap();
    let host_socket = udp_socket().unwrap();
    let flash_view = flash.clone();
    let host = ctl.run(&host_socket, &host_socket, async {
        let operational = commission(
            &ctl,
            Address::Udp(device_addr),
            PASSCODE,
            node_id,
            None,
            || async { Ok(device_addr) },
        )
        .await?;
        assert_eq!(operational, device_addr);

        let mut client =
            Client::connect(ctl.matter, &ctl.crypto, ctl.fab_idx, node_id, operational).await?;
        assert_eq!(client.find_endpoint(None).await?, 1);
        let status = client.status().await?;
        assert_eq!(status.project, "e2e-demo");
        assert_eq!(status.version, "1.0.0");
        assert_eq!(status.chip, Some(Chip::Esp32c6));
        assert_eq!(status.booted, Slot::Ota0);
        assert!(status.confirmed());

        // A foreign project is refused before anything is written.
        let foreign = image("someone-else", "9", 8192);
        let sha: [u8; 32] = Sha256::digest(&foreign).into();
        let error = client.upload(&foreign, &sha, |_, _| {}).await.unwrap_err();
        assert!(format!("{error:#}").contains("refused"), "{error:#}");
        assert_eq!(
            client.status().await?.last_error_text(),
            "image belongs to another project"
        );
        client.abort().await?;
        // Let any session teardown after the refusal finish; the client must
        // reconnect by address on its own.
        async_io::Timer::after(std::time::Duration::from_secs(12)).await;
        assert!(client.status().await?.confirmed());

        // Interrupted upload resumes where the device stopped.
        let update = image("e2e-demo", "2.0.0", 300_001);
        let sha: [u8; 32] = Sha256::digest(&update).into();
        assert_eq!(client.begin(update.len() as u32, &sha).await?.0, 0);
        client.write(0, &update[..1024]).await?;
        client.write(1024, &update[1024..2048]).await?;
        let mut seen = 0;
        let (slot, version) = client.upload(&update, &sha, |sent, _| seen = sent).await?;
        assert_eq!(seen, update.len() as u32);
        assert_eq!(slot, Slot::Ota1);
        assert_eq!(version, "2.0.0");

        let status = client.status().await?;
        assert_eq!(status.staged_version.as_deref(), Some("2.0.0"));

        // `esp-ota share`: a basic window for Home, opened by our admin.
        client.open_basic_window(180).await?;
        Ok(())
    });

    // The app holds off uploads at first (as stillair does while its fan
    // spins down); the host must wait for it rather than fail.
    service.set_accepting(false);
    let release = async {
        async_io::Timer::after(std::time::Duration::from_secs(3)).await;
        service.set_accepting(true);
        core::future::pending::<()>().await
    };

    let mut device = pin!(device);
    let mut host = pin!(host);
    let mut release = pin!(release);
    match select3(&mut device, &mut host, &mut release).await {
        embassy_futures::select::Either3::First(result) => panic!("device stopped: {result:?}"),
        embassy_futures::select::Either3::Second(result) => result.unwrap(),
        embassy_futures::select::Either3::Third(()) => unreachable!(),
    }

    // The slot holds exactly the image and otadata selects it as New.
    let mut flash = flash_view;
    let data = flash.0.borrow().clone();
    let start = 0x10000 + SLOT_LEN as usize;
    let update = image("e2e-demo", "2.0.0", 300_001);
    assert_eq!(&data[start..start + update.len()], &update[..]);
    let layout = Layout::read(&mut flash).unwrap();
    let otadata = OtaData::read(&mut flash, &layout.otadata).unwrap();
    assert_eq!(otadata.selected_slot(), Slot::Ota1);
    assert_eq!(otadata.state_of(Slot::Ota1), Some(ImageState::New));
    let _ = store;
}
