//! `esp-ota`: enroll ESP32 Matter devices into a private fabric and update
//! their firmware over the air.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use esp_lan_ota::{Chip, Slot};
use rs_matter::crypto::Crypto;
use rs_matter::transport::network::Address;
use rs_matter::BasicCommData;

use esp_ota::client::{Client, DeviceStatus};
use esp_ota::commission::{commission, parse_manual_code, WifiCredentials};
use esp_ota::controller::{self, new_fabric, udp_socket, Controller};
use esp_ota::discover::{self, Discriminator};
use esp_ota::store::{default_path, DeviceRecord, Store};
use esp_ota::{image, serial};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Fabric store (default ~/.config/esp-lan-ota/fabric.toml, or $ESP_OTA_STORE).
    #[arg(long, global = true)]
    store: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Add a device to the esp-ota fabric. Run right after a USB flash that
    /// erased otadata; the device prints its setup code on the console.
    Enroll {
        /// Name to use in later commands.
        name: String,
        /// USB serial port to read the enrollment line from (default: the only USB port).
        #[arg(long)]
        serial: Option<String>,
        /// Manual pairing code instead of reading the console.
        #[arg(long)]
        code: Option<String>,
        /// Device address (`ip` or `ip:port`) instead of mDNS discovery.
        #[arg(long)]
        address: Option<String>,
        /// Wi-Fi for a device that has never joined a network (Bluetooth commissioning).
        #[arg(long, env = "ESP_OTA_WIFI_SSID")]
        wifi_ssid: Option<String>,
        #[arg(long, env = "ESP_OTA_WIFI_PASSWORD", hide_env_values = true)]
        wifi_password: Option<String>,
        /// Seconds to wait for the enrollment line or discovery.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    /// Upload firmware, wait for the reboot, and check the new image confirms itself.
    Push {
        name: String,
        /// ELF from cargo, or an app image from `espflash save-image`.
        firmware: PathBuf,
        /// Extra arguments for `espflash save-image` (repeatable).
        #[arg(long = "espflash-arg", allow_hyphen_values = true)]
        espflash_args: Vec<String>,
        /// Seconds to wait for the new image to report itself confirmed.
        #[arg(long, default_value_t = 300)]
        confirm_timeout: u64,
        /// Return after staging, without waiting for reboot and confirmation.
        #[arg(long)]
        no_wait: bool,
    },
    /// Show what a device is running.
    Status { name: String },
    /// List enrolled devices.
    List,
    /// Open a 15-minute pairing window and print the code for Apple Home.
    Share { name: String },
    /// Record a device's new address (normally found automatically).
    SetAddress { name: String, address: String },
    /// Forget a device locally. It keeps our fabric until reset.
    Forget { name: String },
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cli = Cli::parse();
    let path = match cli.store {
        Some(path) => path,
        None => default_path()?,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(cli.command, path))
}

fn load_or_create(path: &std::path::Path) -> Result<Store> {
    if let Some(store) = Store::load(path)? {
        return Ok(store);
    }
    let store = Store {
        fabric: new_fabric(&controller::crypto())?,
        devices: Default::default(),
    };
    store.save(path)?;
    eprintln!("Created a new esp-ota fabric in {}", path.display());
    Ok(store)
}

fn parse_addr(text: &str) -> Result<SocketAddr> {
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return Ok(addr);
    }
    let ip: std::net::IpAddr = text.parse().context("expected ip or ip:port")?;
    Ok(SocketAddr::new(ip, rs_matter::MATTER_PORT))
}

async fn run(command: Command, path: PathBuf) -> Result<()> {
    match command {
        Command::List => {
            let Some(store) = Store::load(&path)? else {
                println!("No fabric yet; `esp-ota enroll` creates one.");
                return Ok(());
            };
            for (name, device) in &store.devices {
                println!(
                    "{name}\t{}\t{}\t{}",
                    device.project.as_deref().unwrap_or("?"),
                    device.chip.as_deref().unwrap_or("?"),
                    device.address.as_deref().unwrap_or("?"),
                );
            }
            Ok(())
        }
        Command::Forget { name } => {
            let mut store = Store::load(&path)?.context("no fabric")?;
            store.devices.remove(&name).context("unknown device")?;
            store.save(&path)?;
            println!("Forgot {name}. The device still trusts the esp-ota fabric.");
            Ok(())
        }
        Command::SetAddress { name, address } => {
            let mut store = Store::load(&path)?.context("no fabric")?;
            let addr = parse_addr(&address)?;
            store.devices.get_mut(&name).context("unknown device")?.address = Some(addr.to_string());
            store.save(&path)?;
            Ok(())
        }
        Command::Enroll {
            name,
            serial,
            code,
            address,
            wifi_ssid,
            wifi_password,
            timeout,
        } => {
            let mut store = load_or_create(&path)?;
            if store.devices.contains_key(&name) {
                bail!("{name} is already enrolled; `esp-ota forget {name}` first");
            }
            let wifi = match (wifi_ssid, wifi_password) {
                (Some(ssid), Some(password)) => Some(WifiCredentials { ssid, password }),
                (None, None) => None,
                _ => bail!("give both --wifi-ssid and --wifi-password"),
            };
            enroll(&mut store, &path, &name, serial, code, address, wifi, Duration::from_secs(timeout)).await
        }
        Command::Status { name } => {
            let mut store = Store::load(&path)?.context("no fabric; enroll a device first")?;
            let ctl = Controller::new(controller::crypto(), &store)?;
            let socket = udp_socket()?;
            let status = ctl
                .run(&socket, &socket, async {
                    let client = connect(&ctl, &mut store, &path, &name).await?;
                    client.status().await
                })
                .await?;
            print_status(&name, &status);
            Ok(())
        }
        Command::Share { name } => {
            let mut store = Store::load(&path)?.context("no fabric; enroll a device first")?;
            let device = store.device(&name)?.clone();
            let ctl = Controller::new(controller::crypto(), &store)?;
            let socket = udp_socket()?;
            ctl.run(&socket, &socket, async {
                let client = connect(&ctl, &mut store, &path, &name).await?;
                client.open_basic_window(900).await
            })
            .await?;
            match (device.passcode, device.discriminator) {
                (Some(passcode), Some(discriminator)) => {
                    let comm = BasicCommData {
                        password: passcode.to_le_bytes().into(),
                        discriminator,
                    };
                    println!(
                        "Pairing window open for 15 minutes. In Home: Add Accessory > More options, code {}",
                        comm.compute_pretty_pairing_code()
                    );
                }
                _ => println!("Pairing window open for 15 minutes; use the device's own setup code."),
            }
            Ok(())
        }
        Command::Push {
            name,
            firmware,
            espflash_args,
            confirm_timeout,
            no_wait,
        } => push(&path, &name, &firmware, &espflash_args, Duration::from_secs(confirm_timeout), no_wait).await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn enroll(
    store: &mut Store,
    path: &std::path::Path,
    name: &str,
    serial_port: Option<String>,
    code: Option<String>,
    address: Option<String>,
    wifi: Option<WifiCredentials>,
    timeout: Duration,
) -> Result<()> {
    let (passcode, discriminator, long_discriminator) = match code {
        Some(code) => {
            let (passcode, short) = parse_manual_code(&code)?;
            (passcode, Discriminator::Short(short), None)
        }
        None => {
            let port = match serial_port {
                Some(port) => port,
                None => serial::default_port()?,
            };
            eprintln!("Waiting for the enrollment line on {port} ...");
            let line = tokio::task::spawn_blocking(move || serial::read_enrollment(&port, timeout))
                .await??;
            (line.passcode, Discriminator::Long(line.discriminator), Some(line.discriminator))
        }
    };

    let device_addr = match address {
        Some(address) => Some(parse_addr(&address)?),
        None => {
            eprintln!("Looking for the device on the LAN ...");
            let window = timeout.min(Duration::from_secs(20));
            tokio::task::spawn_blocking(move || discover::find_commissionable(discriminator, window)).await??
        }
    };

    let node_id = store.allocate_node_id();
    let ctl = Controller::new(controller::crypto(), store)?;
    let socket = udp_socket()?;
    let cfid = ctl.compressed_fabric_id;
    let (operational, status, endpoint) = match device_addr {
        Some(addr) => {
            eprintln!("Commissioning over IP at {addr} ...");
            ctl.run(&socket, &socket, async {
                let operational = commission(&ctl, Address::Udp(controller::peer(addr)), passcode, node_id, None, || async {
                    Ok(addr)
                })
                .await?;
                finish_enrollment(&ctl, node_id, operational).await
            })
            .await?
        }
        None => {
            let Some(wifi) = wifi else {
                bail!(
                    "the device is not on the LAN; for a device that has never joined Wi-Fi pass --wifi-ssid and --wifi-password to commission over Bluetooth"
                );
            };
            enroll_ble(&ctl, &socket, discriminator, passcode, node_id, cfid, &wifi, timeout).await?
        }
    };

    let mut record = DeviceRecord::new(node_id);
    record.address = Some(operational.to_string());
    record.passcode = Some(passcode);
    record.discriminator = long_discriminator;
    record.project = Some(status.project.clone());
    record.chip = status.chip.map(|c| c.name().to_owned());
    record.endpoint = Some(endpoint);
    store.devices.insert(name.to_owned(), record);
    store.save(path)?;
    println!("Enrolled {name}: {} {} at {operational}", status.project, status.version);
    if status.rollback == "unsupported" {
        println!("Warning: the bootloader does not roll back; flash the esp-lan-ota bootloader over USB.");
    }
    Ok(())
}

async fn finish_enrollment<C: Crypto>(
    ctl: &Controller<'_, C>,
    node_id: u64,
    operational: SocketAddr,
) -> Result<(SocketAddr, DeviceStatus, u16)> {
    let mut client = Client::connect(ctl.matter, &ctl.crypto, ctl.fab_idx, node_id, operational).await?;
    let endpoint = client.find_endpoint(None).await?;
    let status = client.status().await?;
    Ok((operational, status, endpoint))
}

#[allow(clippy::too_many_arguments)]
async fn enroll_ble<C: Crypto>(
    ctl: &Controller<'_, C>,
    socket: &async_io::Async<std::net::UdpSocket>,
    discriminator: Discriminator,
    passcode: u32,
    node_id: u64,
    cfid: u64,
    wifi: &WifiCredentials,
    timeout: Duration,
) -> Result<(SocketAddr, DeviceStatus, u16)> {
    use esp_ota::ble::{self, BleTransport};
    use rs_matter::transport::network::ChainedNetwork;

    eprintln!("Not on the LAN; commissioning over Bluetooth ...");
    let ble_filter = match discriminator {
        Discriminator::Long(value) => ble::Discriminator::Long(value),
        Discriminator::Short(value) => ble::Discriminator::Short(value),
    };
    let link = BleTransport::open(ble_filter, timeout).await?;
    let ble_addr = link.address();
    let net = ChainedNetwork::new(|addr: &Address| matches!(addr, Address::Btp(_)), &link, socket);
    let result = {
        let task = ctl.run(&net, &net, async {
            let operational = commission(ctl, Address::Btp(ble_addr), passcode, node_id, Some(wifi), || async {
                eprintln!("Device joined Wi-Fi; finding it on the LAN ...");
                tokio::task::spawn_blocking(move || {
                    discover::find_operational(cfid, node_id, Duration::from_secs(60))
                })
                .await??
                .context("the device did not appear on the LAN after joining Wi-Fi")
            })
            .await?;
            finish_enrollment(ctl, node_id, operational).await
        });
        let pump = link.run();
        match embassy_futures::select::select(task, pump).await {
            embassy_futures::select::Either::First(result) => result,
            embassy_futures::select::Either::Second(result) => {
                Err(result.err().unwrap_or_else(|| anyhow::anyhow!("Bluetooth link closed")))
            }
        }
    };
    link.close().await;
    result
}

/// Connects to an enrolled device, finding a new address over mDNS when the
/// stored one stops answering.
async fn connect<'a, C: Crypto>(
    ctl: &'a Controller<'a, C>,
    store: &mut Store,
    path: &std::path::Path,
    name: &str,
) -> Result<Client<'a, C>> {
    let device = store.device(name)?.clone();
    let stored = device.address.as_deref().map(parse_addr).transpose()?;
    if let Some(addr) = stored {
        let attempt = async {
            Client::connect(ctl.matter, &ctl.crypto, ctl.fab_idx, device.node_id, addr).await
        };
        let timeout = async_io::Timer::after(Duration::from_secs(15));
        if let embassy_futures::select::Either::First(Ok(mut client)) =
            embassy_futures::select::select(attempt, timeout).await
        {
            client.endpoint = device.endpoint.unwrap_or(1);
            return Ok(client);
        }
    }
    let (cfid, node_id) = (ctl.compressed_fabric_id, device.node_id);
    let found = tokio::task::spawn_blocking(move || {
        discover::find_operational(cfid, node_id, Duration::from_secs(10))
    })
    .await??
    .with_context(|| format!("{name} did not answer and was not found over mDNS"))?;
    let mut client = Client::connect(ctl.matter, &ctl.crypto, ctl.fab_idx, device.node_id, found).await?;
    client.endpoint = device.endpoint.unwrap_or(1);
    if let Some(record) = store.devices.get_mut(name) {
        record.address = Some(found.to_string());
    }
    store.save(path)?;
    Ok(client)
}

fn print_status(name: &str, status: &DeviceStatus) {
    println!("{name}: {} {}", status.project, status.version);
    println!(
        "  chip {}  slot {}  image {}  rollback {}",
        status.chip.map_or("?", Chip::name),
        status.booted.name(),
        status.state.map_or("unrecorded", |s| s.name()),
        status.rollback
    );
    if let Some((len, next)) = status.upload {
        println!("  upload in progress: {next}/{len} bytes");
    }
    if let Some(version) = &status.staged_version {
        println!("  staged {version}; reboot pending");
    }
    if status.last_error != 0 {
        println!("  last error: {}", status.last_error_text());
    }
}

async fn push(
    path: &std::path::Path,
    name: &str,
    firmware: &std::path::Path,
    espflash_args: &[String],
    confirm_timeout: Duration,
    no_wait: bool,
) -> Result<()> {
    let mut store = Store::load(path)?.context("no fabric; enroll a device first")?;
    let chip = store
        .device(name)?
        .chip
        .as_deref()
        .and_then(Chip::from_name)
        .context("unknown chip; run `esp-ota status` first")?;
    let image = image::load(firmware, chip, espflash_args)?;
    let ctl = Controller::new(controller::crypto(), &store)?;
    let socket = udp_socket()?;
    let new_version = image.info.version.as_str().to_owned();

    ctl.run(&socket, &socket, async {
        let client = connect(&ctl, &mut store, path, name).await?;
        let before = client.status().await?;
        if before.chip != Some(image.info.chip) {
            bail!(
                "image is for {}, device is {:?}",
                image.info.chip.name(),
                before.chip.map(Chip::name)
            );
        }
        if before.project != image.info.project.as_str() {
            bail!(
                "image project {:?} does not match device project {:?}",
                image.info.project.as_str(),
                before.project
            );
        }
        if !before.confirmed() {
            bail!("the running image has not confirmed itself yet; wait for it or check the device");
        }
        eprintln!(
            "{name}: {} -> {} ({} KiB)",
            before.version,
            new_version,
            image.bytes.len() / 1024
        );
        let started = Instant::now();
        let mut last_report = Instant::now();
        let (staged_slot, version) = client
            .upload(&image.bytes, &image.sha256, |sent, total| {
                if last_report.elapsed() > Duration::from_secs(2) || sent == total {
                    last_report = Instant::now();
                    eprint!(
                        "\r  {:>3}%  {} / {} KiB",
                        u64::from(sent) * 100 / u64::from(total),
                        sent / 1024,
                        total / 1024
                    );
                }
            })
            .await?;
        eprintln!(
            "\n  verified and staged {version} in {} after {:.0?}",
            staged_slot.name(),
            started.elapsed()
        );
        if no_wait {
            println!("Staged {new_version}; the device reboots into it now.");
            return Ok(());
        }
        drop(client);
        wait_for_confirmation(&ctl, &mut store, path, name, &before, staged_slot, &new_version, confirm_timeout).await
    })
    .await
}

#[allow(clippy::too_many_arguments)]
async fn wait_for_confirmation<C: Crypto>(
    ctl: &Controller<'_, C>,
    store: &mut Store,
    path: &std::path::Path,
    name: &str,
    before: &DeviceStatus,
    staged_slot: Slot,
    new_version: &str,
    confirm_timeout: Duration,
) -> Result<()> {
    eprintln!("Waiting for {name} to reboot and confirm ...");
    let deadline = Instant::now() + confirm_timeout;
    let mut seen_new = false;
    loop {
        if Instant::now() > deadline {
            if seen_new {
                bail!("{name} runs {new_version} but has not confirmed it; it rolls back on its next reset");
            }
            bail!("{name} did not come back within {}s", confirm_timeout.as_secs());
        }
        async_io::Timer::after(Duration::from_secs(4)).await;
        let probe = async {
            let client = connect(ctl, store, path, name).await?;
            client.status().await
        };
        let Ok(status) = probe.await else { continue };
        if status.version == new_version && status.booted == staged_slot {
            seen_new = true;
            if status.confirmed() {
                println!(
                    "{name} is running {new_version} from {} and confirmed it.",
                    staged_slot.name()
                );
                if status.rollback == "unsupported" {
                    println!("Warning: this bootloader would not have rolled back a bad image.");
                }
                return Ok(());
            }
        } else if status.booted == before.booted && status.staged_version.is_none() {
            bail!(
                "{name} is back on {} in {}: the update did not stick (rolled back)",
                status.version,
                status.booted.name()
            );
        }
    }
}

