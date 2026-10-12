# esp-lan-ota

OTA updates for Michael's ESP32 Rust firmware over Matter. Firmware projects
(key-right C3, stillair C6, crystal-shim C6) depend on `crates/esp-lan-ota` by
git rev; the host CLI is `crates/esp-ota`.

## Rules

- The device crate stays `no_std`, allocation-free, and free of esp-* crates.
  Chip access arrives through `embedded-storage` traits and values the app
  passes in. Matter glue lives behind a per-version feature
  (`rs-matter-0-2`); add `rs-matter-0-3` beside it rather than bumping.
- Never patch or fork rs-matter, rs-matter-stack or rs-matter-embassy. Use
  their public APIs.
- `otadata` changes go to the non-active sector with a higher sequence number;
  never rewrite the active record. Keep `src/otadata.rs` tests passing,
  including the bootloader model and power-loss cases.
- The cluster ID, command IDs and field tags are a wire protocol shared by
  deployed firmware and the CLI. Add fields with new tags; do not renumber.
- Memory matters on C3 firmware: no new static buffers or heap in the device
  crate.
- `docs/design.md` owns the protocol and boot-selection rules;
  `docs/integration.md` owns the firmware integration steps.

## Checks

```sh
sh scripts/check.sh
```

Runs fmt, clippy, all host tests (including `crates/esp-ota/tests/e2e.rs`,
the in-process enroll-and-update test), and `no_std` builds for the RISC-V
targets. CI adds an Xtensa (S3) build. Bootloaders are built by the
`bootloaders` workflow and committed under `bootloaders/`.

Hardware behaviour (real flash, Wi-Fi, Bluetooth, the bootloader) is not
covered by host tests; record hardware results in the firmware project.
