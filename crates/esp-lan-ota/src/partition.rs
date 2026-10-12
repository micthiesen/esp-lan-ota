//! Finds the OTA partitions in the ESP-IDF partition table.
//!
//! The table lives at 0x8000 as 32-byte entries: magic `AA 50`, type,
//! subtype, offset, size, 16-byte label, flags. It ends at an erased entry or
//! the `EB EB` MD5 entry. Entries are read one at a time, so no 3 KiB table
//! buffer is needed.

use embedded_storage::nor_flash::NorFlash;

use crate::flash::{FlashError, Partition, SECTOR};
use crate::otadata::{Slot, OTADATA_LEN};

pub const TABLE_OFFSET: u32 = 0x8000;
pub const TABLE_MAX_LEN: u32 = 0xC00;
const ENTRY_LEN: u32 = 32;
const ENTRY_MAGIC: [u8; 2] = [0xAA, 0x50];

const TYPE_APP: u8 = 0x00;
const TYPE_DATA: u8 = 0x01;
const SUBTYPE_OTA_0: u8 = 0x10;
const SUBTYPE_OTA_1: u8 = 0x11;
const SUBTYPE_OTADATA: u8 = 0x00;
const SUBTYPE_FACTORY: u8 = 0x00;

/// The partitions esp-lan-ota uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub otadata: Partition,
    pub slots: [Partition; 2],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutError {
    Flash(FlashError),
    /// The table has no `otadata`, `ota_0` or `ota_1` partition.
    Missing,
    /// A partition is misaligned, overlaps another, or `otadata` is not 8 KiB.
    Invalid,
}

impl From<FlashError> for LayoutError {
    fn from(error: FlashError) -> Self {
        LayoutError::Flash(error)
    }
}

impl Layout {
    pub fn read<F: NorFlash>(flash: &mut F) -> Result<Self, LayoutError> {
        let table = Partition {
            offset: TABLE_OFFSET,
            len: TABLE_MAX_LEN,
        };
        let mut otadata = None;
        let mut slots = [None; 2];
        let mut others = [None::<Partition>; 16];
        let mut other_count = 0;
        let mut factory = false;
        let mut raw = [0u8; ENTRY_LEN as usize];
        for index in 0..TABLE_MAX_LEN / ENTRY_LEN {
            table.read(flash, index * ENTRY_LEN, &mut raw)?;
            if raw[0..2] != ENTRY_MAGIC {
                break;
            }
            let word =
                |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
            let partition = Partition {
                offset: word(4),
                len: word(8),
            };
            match (raw[2], raw[3]) {
                (TYPE_DATA, SUBTYPE_OTADATA) => otadata = Some(partition),
                (TYPE_APP, SUBTYPE_OTA_0) => slots[0] = Some(partition),
                (TYPE_APP, SUBTYPE_OTA_1) => slots[1] = Some(partition),
                (TYPE_APP, SUBTYPE_FACTORY) => factory = true,
                _ => {
                    if let Some(free) = others.get_mut(other_count) {
                        *free = Some(partition);
                        other_count += 1;
                    }
                }
            }
        }
        let (Some(otadata), [Some(ota_0), Some(ota_1)]) = (otadata, slots) else {
            return Err(LayoutError::Missing);
        };
        // With a factory app the bootloader falls back to it rather than to
        // ota_0, which breaks the USB-flash and rollback assumptions.
        if factory {
            return Err(LayoutError::Invalid);
        }
        let layout = Layout {
            otadata,
            slots: [ota_0, ota_1],
        };
        layout.validate()?;
        // Never let a write to otadata or a slot reach nvs or anything else.
        for other in others.iter().flatten() {
            for ours in [layout.otadata, layout.slots[0], layout.slots[1]] {
                let other_end = other.offset.saturating_add(other.len);
                if ours.offset < other_end && other.offset < ours.offset + ours.len {
                    return Err(LayoutError::Invalid);
                }
            }
        }
        Ok(layout)
    }

    fn validate(&self) -> Result<(), LayoutError> {
        if self.otadata.len != OTADATA_LEN {
            return Err(LayoutError::Invalid);
        }
        let all = [self.otadata, self.slots[0], self.slots[1]];
        for a in &all {
            if !a.offset.is_multiple_of(SECTOR)
                || !a.len.is_multiple_of(SECTOR)
                || a.len == 0
                || a.offset.checked_add(a.len).is_none()
            {
                return Err(LayoutError::Invalid);
            }
        }
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                if a.offset < b.offset + b.len && b.offset < a.offset + a.len {
                    return Err(LayoutError::Invalid);
                }
            }
        }
        Ok(())
    }

    pub fn slot(&self, slot: Slot) -> Partition {
        self.slots[slot.index()]
    }

    /// The slot whose partition starts at `offset`.
    pub fn slot_at(&self, offset: u32) -> Option<Slot> {
        [Slot::Ota0, Slot::Ota1]
            .into_iter()
            .find(|slot| self.slot(*slot).offset == offset)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::flash::mock::MockFlash;

    fn entry(kind: u8, subtype: u8, offset: u32, len: u32, label: &str) -> [u8; 32] {
        let mut raw = [0xFFu8; 32];
        raw[0..2].copy_from_slice(&ENTRY_MAGIC);
        raw[2] = kind;
        raw[3] = subtype;
        raw[4..8].copy_from_slice(&offset.to_le_bytes());
        raw[8..12].copy_from_slice(&len.to_le_bytes());
        raw[12..28].fill(0);
        raw[12..12 + label.len()].copy_from_slice(label.as_bytes());
        raw[28..32].copy_from_slice(&0u32.to_le_bytes());
        raw
    }

    /// A small table that fits the 128 KiB mock flash.
    pub(crate) fn write_test_table(flash: &mut MockFlash) -> Layout {
        let entries = [
            entry(TYPE_DATA, 0x02, 0x9000, 0x4000, "nvs"),
            entry(TYPE_DATA, SUBTYPE_OTADATA, 0xD000, 0x2000, "otadata"),
            entry(TYPE_APP, SUBTYPE_OTA_0, 0x10000, 0x8000, "ota_0"),
            entry(TYPE_APP, SUBTYPE_OTA_1, 0x18000, 0x8000, "ota_1"),
        ];
        for (i, raw) in entries.iter().enumerate() {
            let at = TABLE_OFFSET as usize + i * 32;
            flash.data[at..at + 32].copy_from_slice(raw);
        }
        // MD5 entry terminates the table.
        let at = TABLE_OFFSET as usize + entries.len() * 32;
        flash.data[at..at + 2].copy_from_slice(&[0xEB, 0xEB]);
        Layout::read(flash).unwrap()
    }

    #[test]
    fn reads_ota_layout() {
        let mut flash = MockFlash::new();
        let layout = write_test_table(&mut flash);
        assert_eq!(
            layout.otadata,
            Partition {
                offset: 0xD000,
                len: 0x2000
            }
        );
        assert_eq!(
            layout.slot(Slot::Ota1),
            Partition {
                offset: 0x18000,
                len: 0x8000
            }
        );
        assert_eq!(layout.slot_at(0x10000), Some(Slot::Ota0));
        assert_eq!(layout.slot_at(0x20000), None);
    }

    #[test]
    fn rejects_factory_only_and_overlapping_tables() {
        let mut flash = MockFlash::new();
        let at = TABLE_OFFSET as usize;
        flash.data[at..at + 32]
            .copy_from_slice(&entry(TYPE_APP, 0x00, 0x10000, 0x10000, "factory"));
        assert_eq!(Layout::read(&mut flash), Err(LayoutError::Missing));

        let mut flash = MockFlash::new();
        let entries = [
            entry(TYPE_DATA, SUBTYPE_OTADATA, 0xD000, 0x2000, "otadata"),
            entry(TYPE_APP, SUBTYPE_OTA_0, 0x10000, 0x9000, "ota_0"),
            entry(TYPE_APP, SUBTYPE_OTA_1, 0x18000, 0x8000, "ota_1"),
        ];
        for (i, raw) in entries.iter().enumerate() {
            flash.data[at + i * 32..at + i * 32 + 32].copy_from_slice(raw);
        }
        assert_eq!(Layout::read(&mut flash), Err(LayoutError::Invalid));
    }

    #[test]
    fn rejects_factory_apps_and_overlap_with_nvs() {
        let at = TABLE_OFFSET as usize;
        let write = |flash: &mut MockFlash, entries: &[[u8; 32]]| {
            for (i, raw) in entries.iter().enumerate() {
                flash.data[at + i * 32..at + i * 32 + 32].copy_from_slice(raw);
            }
        };
        let mut flash = MockFlash::new();
        write(
            &mut flash,
            &[
                entry(TYPE_DATA, SUBTYPE_OTADATA, 0xD000, 0x2000, "otadata"),
                entry(TYPE_APP, SUBTYPE_FACTORY, 0x8000_0, 0x1000, "factory"),
                entry(TYPE_APP, SUBTYPE_OTA_0, 0x10000, 0x8000, "ota_0"),
                entry(TYPE_APP, SUBTYPE_OTA_1, 0x18000, 0x8000, "ota_1"),
            ],
        );
        assert_eq!(Layout::read(&mut flash), Err(LayoutError::Invalid));

        let mut flash = MockFlash::new();
        write(
            &mut flash,
            &[
                entry(TYPE_DATA, 0x02, 0xC000, 0x2000, "nvs"),
                entry(TYPE_DATA, SUBTYPE_OTADATA, 0xD000, 0x2000, "otadata"),
                entry(TYPE_APP, SUBTYPE_OTA_0, 0x10000, 0x8000, "ota_0"),
                entry(TYPE_APP, SUBTYPE_OTA_1, 0x18000, 0x8000, "ota_1"),
            ],
        );
        assert_eq!(Layout::read(&mut flash), Err(LayoutError::Invalid));
    }

    #[test]
    fn parses_key_right_ota_layout() {
        // The planned key-right table keeps nvs where Home pairing lives.
        let mut flash = crate::flash::mock::MockFlash::new();
        let at = TABLE_OFFSET as usize;
        let entries = [
            entry(TYPE_DATA, 0x02, 0x9000, 0x10000, "nvs"),
            entry(TYPE_DATA, SUBTYPE_OTADATA, 0x19000, 0x2000, "otadata"),
            entry(TYPE_APP, SUBTYPE_OTA_0, 0x20000, 0x1F0000, "ota_0"),
            entry(TYPE_APP, SUBTYPE_OTA_1, 0x210000, 0x1F0000, "ota_1"),
        ];
        for (i, raw) in entries.iter().enumerate() {
            flash.data[at + i * 32..at + i * 32 + 32].copy_from_slice(raw);
        }
        let layout = Layout::read(&mut flash).unwrap();
        assert_eq!(layout.slot(Slot::Ota0).len, 0x1F0000);
        assert_eq!(layout.slot_at(0x210000), Some(Slot::Ota1));
    }
}
