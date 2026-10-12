# Design

## Transport: the device's own Matter stack

Firmware already runs rs-matter, which owns the network stack and has no spare
sockets. Rather than patching it, updates travel as ordinary Matter commands
to a manufacturer-specific cluster (`0xFFF1_FC70`) over an encrypted CASE
session. The device needs no extra socket and no extra buffers, and the same
path works over Wi-Fi and Thread. Every command requires Administer privilege,
so only administrators on one of the device's fabrics can update it.

| Command | ID | Fields | Response |
| --- | --- | --- | --- |
| Status | 0x00 | none | 0x80: project, version, chip ID, booted slot, image state, rollback, upload length/offset, staged flag/version, last error, max chunk |
| Begin | 0x01 | 0: length u32, 1: SHA-256 octets | 0x81: next offset u32, max chunk u16; `Busy` while the app holds off uploads |
| Write | 0x02 | 0: offset u32, 1: data octets (≤ 1024) | status |
| Finish | 0x03 | none | 0x83: slot u8, version string |
| Abort | 0x04 | none | status |

Begin with the same length and hash resumes and returns the device's offset;
a different image restarts. A flash error, a resent chunk that differs from
flash, or a hash mismatch drops the upload, so the next Begin starts again from
0. The host aborts on failure and, if a Finish response is lost, checks Status
for the staged version. Writes must be in order at word-aligned offsets;
a resent chunk is compared with flash and accepted again. Refusals carry an IM
status, and `Status.last_error` gives the reason.

## Enrollment: a private fabric

`esp-ota` keeps its own Matter fabric (root CA discarded, intermediate CA and
controller identity in `~/.config/esp-lan-ota/fabric.toml`, mode 0600) and
commissions each device into it with the standard flow. Apple Home and other
fabrics are untouched.

A device opens a 15-minute basic commissioning window only after a USB flash,
and prints `esp-lan-ota enroll passcode=... discriminator=...` on its console.
Physical USB access is the consent. It detects such a boot from `otadata`: the
USB flash erases it and the bootloader writes an unlabelled record, while every
record this crate writes carries `esp-lan-ota` in the `seq_label` field. The
enrollment period ends, by writing an equivalent labelled record, when the
window's 15 minutes have run or the first upload starts; a reset before then
reopens the window.

The window uses the device's own setup passcode (rs-matter 0.2 exposes only
the basic window publicly), so devices should use random per-device passcodes.

A device that has never joined Wi-Fi is commissioned over Bluetooth (BTP) with
Wi-Fi credentials. rs-matter 0.2 does not expose its BTP central role, so
`esp-ota` implements it.

`esp-ota share` opens a basic window through AdministratorCommissioning and
prints the device's own setup code for Home. Removing the device from Home
removes only Home's fabric.

## Boot selection (`otadata`)

The ESP-IDF bootloader starts the bootable record with the highest sequence
number (`slot = (seq - 1) % 2`). With `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE` it
turns `New` into `PendingVerify` on the first boot and `PendingVerify` into
`Aborted` at the next boot, falling back to the other record.

This crate never rewrites the active record. Each change goes into the other
sector with a higher sequence number, so power loss mid-write leaves the
previous selection in place:

- **Activate**: if the active record does not describe the running slot, first
  write a `Valid` record for it, so a failed update falls back to exactly what
  was running. Then write a `New` record for the target slot.
- **Confirm**: write a `Valid` record for the running slot with sequence
  `active + 2` (same slot). This erases the sector holding the previous
  fallback record. Power loss during that write leaves only the running
  image's `PendingVerify` record, which the bootloader aborts, and with no
  bootable record it starts `ota_0`. ESP-IDF's own mark-valid has the same
  window. The device boots a complete image either way, but possibly the older
  one.
- **Seed before rewriting**: `Begin` first makes the active record describe
  the running slot. After a fallback the active record can still point at the
  failed slot that is about to be erased.

Uploads are refused while the running image is unconfirmed, because the other
slot then holds the only known-good image.

These rules are tested against a model of the ESP-IDF v5.5 bootloader
(`bootloader_utility_get_selected_boot_partition`, `set_actual_ota_seq`),
including power loss at every write.

## Verification

- First chunk: ESP image magic, chip ID, `esp_app_desc_t` magic, project name.
- Every write is read back.
- `Finish` compares the streamed SHA-256, then re-reads the whole slot and
  compares again, before `otadata` changes.
- The bootloader verifies the image's appended SHA-256 on every boot and falls
  back to the other slot if it fails.

## Memory

About 400 bytes of static state. Chunks are written directly from Matter's
receive buffer; the slot re-read uses a 64-byte stack buffer. No heap.
