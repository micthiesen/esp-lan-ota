# Adding esp-lan-ota to a firmware project

For esp-hal + rs-matter 0.2 firmware (rs-matter-stack / rs-matter-embassy).
The result: one last USB flash, then every update goes over Matter with
`esp-ota push`.

## 1. Dependency

```toml
[dependencies]
esp-lan-ota = { git = "https://github.com/micthiesen/esp-lan-ota", rev = "<commit>", features = ["rs-matter-0-2"] }
```

The crate depends only on `embedded-storage 0.3`, `sha2 0.10` and, with the
feature, `rs-matter 0.2` and `embassy-time 0.5` (both already in the
firmware's graph). It has no esp-* dependency and no chip feature.

Keep `esp-storage`'s `critical-section` feature on. It is a default feature,
so a dependency declared with `default-features = false` must list it
explicitly. Updates write hundreds of sectors while Wi-Fi interrupts run;
without it, an interrupt that executes from flash during an erase crashes the
chip.

## 2. Partition table

Use `partitions/<size>mb.csv` from this repo, or add `otadata` and two equal
`ota_0`/`ota_1` slots to the existing table. Rules:

- No `factory` partition.
- `otadata` is exactly `0x2000`.
- App partitions are 64 KiB aligned.
- Do not move or resize `nvs` on a device that is already paired; Matter
  fabrics live there.

Each slot must hold the image with room to grow. Check
`espflash save-image` output against the slot size.

## 3. Bootloader

Flash the rollback-enabled bootloader from `bootloaders/<chip>-bootloader.bin`.
Without it, an image that never confirms itself is not rolled back
(`esp-ota status` reports `rollback unsupported`).

## 4. USB flash command

Every USB flash must erase `otadata`, so the bootloader starts the image
espflash writes to `ota_0`, and so the firmware opens its enrollment window:

```sh
espflash flash --chip <chip> \
  --bootloader <esp-lan-ota>/bootloaders/<chip>-bootloader.bin \
  --partition-table partitions.csv \
  --target-app-partition ota_0 \
  --erase-parts otadata \
  target/<target>/release/<app>
```

## 5. Firmware

Before the Matter stack runs:

```rust
use esp_lan_ota::matter::{announce_enrollment, open_enrollment_window, OtaCluster, OtaService};
use esp_lan_ota::{Chip, Identity, Name, Ota};

esp_bootloader_esp_idf::esp_app_desc!();

// A second handle to the same flash. esp-storage's docs say a second
// `FlashStorage::new` panics, but the code (esp-hal 10e48dd) does not check;
// operations are serialised by its critical section.
let mut flash = esp_storage::FlashStorage::new(unsafe { esp_hal::peripherals::FLASH::steal() });

// Which slot is running (MMU lookup). The 3 KiB buffer is only needed here.
let booted = {
    let mut buf = alloc::vec![0u8; esp_bootloader_esp_idf::partitions::PARTITION_TABLE_MAX_LEN];
    let buf: &mut [u8; esp_bootloader_esp_idf::partitions::PARTITION_TABLE_MAX_LEN] =
        buf.as_mut_slice().try_into().unwrap();
    esp_bootloader_esp_idf::partitions::read_partition_table(&mut flash, buf)?
        .booted_partition()?
        .map(|p| p.offset())
};

let identity = Identity {
    chip: Chip::Esp32c3, // the project's chip

    project: Name::new(ESP_APP_DESC.project_name()).unwrap(),
    version: Name::new(ESP_APP_DESC.version()).unwrap(),
};
// Err: not an OTA layout (for example after a bare `espflash flash`).
// Prefer running without OTA over not running at all.
let ota = Ota::start(flash, booted.unwrap(), identity)?;
static OTA: StaticCell<OtaService<FlashStorage>> = StaticCell::new();
let ota = &*OTA.init(OtaService::new(ota));
```

`Ota::start` reads about 100 bytes of flash and keeps about 400 bytes of
state. Uploads use no further RAM: chunks are written straight from Matter's
receive buffer.

If `Ota::start` fails, keep the cluster declared with
`OtaCluster::unavailable(dataver)` so the data model is the same; it refuses
every command.

Add the cluster to the app's main endpoint (not endpoint 0, which
rs-matter-stack handles itself) and chain the handler:

```rust
Endpoint::new(1, devices!(...), clusters!(..., OtaCluster::<'static, FlashStorage>::CLUSTER))

handler.chain(
    EpClMatcher::new(Some(1), Some(esp_lan_ota::matter::CLUSTER_ID)),
    Async(OtaCluster::new(ota, Dataver::new_rand(&mut rand))),
)
```

After the stack has loaded its persisted state, open the enrollment window
(only after a USB flash) and print the line `esp-ota enroll` reads:

```rust
if let Some(enrollment) = open_enrollment_window(&ota.boot_info(), stack.matter(), &crypto)? {
    // run alongside the stack; ends the enrollment period after 15 minutes
    announce_enrollment(ota, enrollment, |e| println!("{e}")).await;
}
```

The enrollment period lasts until the window's 15 minutes have run or the
first upload starts, so a reset in between (serial monitor, brownout) reopens
the window. The window uses the device's own setup passcode: give each device
a random passcode, not the public test passcode `20202021`, or anyone on the
LAN can add a fabric during those 15 minutes.

Run these alongside the stack:

```rust
// Reboot into a staged update.
async {
    ota.wait_reboot().await;
    esp_hal::system::software_reset();
}

// Confirm the running image once the app is healthy; until then the
// bootloader rolls back on the next reset and uploads are refused.
async {
    if ota.is_unconfirmed() {
        match select(app_healthy(), Timer::after_secs(300)).await {
            Either::First(()) => { ota.confirm()?; }
            Either::Second(()) => esp_hal::system::software_reset(), // roll back
        }
    }
}
```

Pick a health check that would fail on a broken build: Matter operational
(network up, sessions served) plus the project's own peripherals currently
working (not just "worked once"). A watchdog reset before confirmation also
rolls back.

Hardware that must be in a safe state before a reboot can hold off uploads
and react when one starts:

```rust
ota.set_accepting(false);          // Begin answers Busy; esp-ota retries for 2 min
ota.wait_upload_started().await;   // a fresh upload began
ota.is_updating()                  // upload in progress or staged
```

`Finish` re-reads and hashes the whole image inside the Matter handler, which
blocks the executor for about a second per MiB on a C3. Each 1 KiB chunk
write, and the 4 KiB erase before every fourth chunk, also runs inline. Keep
watchdog timeouts well above that.

## 6. Enroll and push

```sh
espflash flash ... --erase-parts otadata        # step 4, last USB flash
esp-ota enroll lamp-a                             # reads the console line, commissions over IP
esp-ota push lamp-a target/<target>/release/<app>
esp-ota share lamp-a                              # optional: code for Apple Home
```

A device that has never joined Wi-Fi is enrolled over Bluetooth instead:
`esp-ota enroll <name> --wifi-ssid ... --wifi-password ...`.

## What to verify on hardware

- First boot after the USB flash prints the enrollment line; `esp-ota enroll`
  succeeds; existing Home fabrics still work.
- `esp-ota push` completes, the device reboots into the other slot, confirms,
  and `esp-ota status` shows `image valid` and `rollback supported`.
- A build that never confirms rolls back after its reset (for example a test
  build that skips `confirm`).
