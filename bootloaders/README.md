# Rollback-enabled bootloaders

ESP-IDF second-stage bootloaders for esp-lan-ota, built by the `bootloaders`
workflow from `project/` with ESP-IDF `release/v5.5` (`v5.5.5-832-g2553c5ad432`,
GitHub Actions run 38320654751). They match espflash's bundled bootloaders
(ESP-IDF v5.5 defaults; 64 MB flash-size setting on C6/H2 for esp-hal's 64 KiB
MMU page) plus `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y`:

- an image activated as `New` becomes `PendingVerify` when it first boots;
- an image still `PendingVerify` at the next boot becomes `Aborted`, and the
  previous slot starts.

| File | Chip |
| --- | --- |
| `esp32c3-bootloader.bin` | ESP32-C3 |
| `esp32c6-bootloader.bin` | ESP32-C6 |
| `esp32h2-bootloader.bin` | ESP32-H2 |
| `esp32s3-bootloader.bin` | ESP32-S3 |

Checksums are in `SHA256SUMS`. Flash with
`espflash flash --bootloader bootloaders/<chip>-bootloader.bin ...`; espflash
patches the flash mode, speed and size fields in the header for the target.

To rebuild, run the `bootloaders` workflow and replace the files and checksums.
