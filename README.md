# esp-lan-ota

Over-the-air firmware updates for ESP32 Rust (esp-hal + rs-matter) projects,
delivered over the device's own Matter connection. One USB flash, then:

```sh
esp-ota enroll lamp          # once, right after the USB flash
esp-ota push lamp target/riscv32imc-unknown-none-elf/release/lamp
```

`push` builds the app image, uploads it through an encrypted Matter session,
waits for the reboot, and checks the new image confirmed itself. A build that
never confirms is rolled back by the bootloader.

Supported chips: ESP32-C3, C6, H2 (Thread) and S3.

## Pieces

- `crates/esp-lan-ota`: the `no_std` device crate. Partition table, image
  checks, power-loss-safe boot selection, a verified slot writer, and (feature
  `rs-matter-0-2`) a Matter cluster. No esp-* dependency; about 400 bytes of
  RAM.
- `crates/esp-ota`: the host CLI. Keeps a private Matter fabric, commissions
  devices over IP or Bluetooth, and pushes updates.
- `bootloaders/`: ESP-IDF v5.5 bootloaders with app rollback enabled, built by
  CI.
- `partitions/`: two-slot layouts for 4, 8 and 16 MiB flash.

## Install

```sh
cargo install --git https://github.com/micthiesen/esp-lan-ota esp-ota
```

`espflash` must be on `PATH` to convert ELF files.

## Commands

| Command | What it does |
| --- | --- |
| `esp-ota enroll <name>` | Reads the device's enrollment line from USB serial (or `--code`), finds it over mDNS (or `--address`), and commissions it into the esp-ota fabric. `--wifi-ssid/--wifi-password` commission a never-networked device over Bluetooth. |
| `esp-ota push <name> <elf\|bin>` | Uploads, verifies, stages, waits for reboot and confirmation. |
| `esp-ota status <name>` | Project, version, slot, image state, rollback support. |
| `esp-ota share <name>` | Opens a 15-minute window and prints the setup code for Apple Home. |
| `esp-ota list` / `set-address` / `forget` | Manage the local device list. |

## Docs

- [docs/integration.md](docs/integration.md): adding it to a firmware project.
- [docs/design.md](docs/design.md): protocol, enrollment, boot selection,
  verification.

## Status

Host-tested: the device crate (unit tests, including a model of the ESP-IDF
rollback bootloader with power loss at every write) and an in-process end to
end test where the real host commissions an rs-matter device running the real
cluster and updates it. Not yet exercised on hardware.

Licensed under MIT or Apache-2.0.
