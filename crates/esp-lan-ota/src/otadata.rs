//! The ESP-IDF `otadata` partition: which app slot the bootloader starts.
//!
//! The partition holds two 32-byte `esp_ota_select_entry_t` records, one per
//! 4 KiB sector. The bootloader picks the bootable record with the highest
//! sequence number and starts slot `(seq - 1) % 2`. With
//! `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE` it turns `New` into
//! `PendingVerify` on first boot and turns any `PendingVerify` it finds at the
//! next boot into `Aborted`, which makes the record unbootable and falls back
//! to the other one.
//!
//! This module never rewrites the record the bootloader currently uses. Each
//! change goes to the other sector with a higher sequence number, so losing
//! power mid-write leaves the previous selection intact.

use embedded_storage::nor_flash::NorFlash;

use crate::flash::{FlashError, Partition, SECTOR};

pub const ENTRY_LEN: usize = 32;
pub const OTADATA_LEN: u32 = 2 * SECTOR;

const ERASED_SEQ: u32 = u32::MAX;

/// One of the two OTA app slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Slot {
    Ota0,
    Ota1,
}

impl Slot {
    pub const fn index(self) -> usize {
        match self {
            Slot::Ota0 => 0,
            Slot::Ota1 => 1,
        }
    }

    pub const fn other(self) -> Slot {
        match self {
            Slot::Ota0 => Slot::Ota1,
            Slot::Ota1 => Slot::Ota0,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Slot::Ota0 => "ota_0",
            Slot::Ota1 => "ota_1",
        }
    }

    pub fn from_name(name: &str) -> Option<Slot> {
        match name {
            "ota_0" => Some(Slot::Ota0),
            "ota_1" => Some(Slot::Ota1),
            _ => None,
        }
    }

    /// The slot the bootloader maps a sequence number to.
    pub const fn for_seq(seq: u32) -> Slot {
        if seq.wrapping_sub(1).is_multiple_of(2) {
            Slot::Ota0
        } else {
            Slot::Ota1
        }
    }
}

/// `esp_ota_img_states_t`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageState {
    New,
    PendingVerify,
    Valid,
    Invalid,
    Aborted,
    Undefined,
    Unknown(u32),
}

impl ImageState {
    pub const fn from_raw(raw: u32) -> Self {
        match raw {
            0 => ImageState::New,
            1 => ImageState::PendingVerify,
            2 => ImageState::Valid,
            3 => ImageState::Invalid,
            4 => ImageState::Aborted,
            0xFFFF_FFFF => ImageState::Undefined,
            other => ImageState::Unknown(other),
        }
    }

    pub const fn raw(self) -> u32 {
        match self {
            ImageState::New => 0,
            ImageState::PendingVerify => 1,
            ImageState::Valid => 2,
            ImageState::Invalid => 3,
            ImageState::Aborted => 4,
            ImageState::Undefined => 0xFFFF_FFFF,
            ImageState::Unknown(raw) => raw,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            ImageState::New => "new",
            ImageState::PendingVerify => "pending-verify",
            ImageState::Valid => "valid",
            ImageState::Invalid => "invalid",
            ImageState::Aborted => "aborted",
            ImageState::Undefined => "undefined",
            ImageState::Unknown(_) => "unknown",
        }
    }

    /// The image has been started but not yet confirmed by the app.
    pub const fn is_unconfirmed(self) -> bool {
        matches!(self, ImageState::New | ImageState::PendingVerify)
    }
}

/// CRC the bootloader stores over the little-endian sequence number
/// (`esp_rom_crc32_le(UINT32_MAX, &seq, 4)`).
pub fn seq_crc(seq: u32) -> u32 {
    let mut crc = 0u32;
    for byte in seq.to_le_bytes() {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Written into `seq_label` of every record this crate writes.
pub const MARKER: [u8; 11] = *b"esp-lan-ota";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub seq: u32,
    pub state: ImageState,
    pub crc: u32,
    pub marked: bool,
}

impl Entry {
    pub const ERASED: Entry = Entry {
        seq: ERASED_SEQ,
        state: ImageState::Undefined,
        crc: ERASED_SEQ,
        marked: false,
    };

    /// A record written by this crate. The `seq_label` field, which the
    /// bootloader ignores and preserves, carries [`MARKER`] so records the
    /// bootloader created itself (after a USB flash) can be told apart.
    pub fn new(seq: u32, state: ImageState) -> Self {
        Self {
            seq,
            state,
            crc: seq_crc(seq),
            marked: true,
        }
    }

    /// A record as ESP-IDF tools write it, with an erased label.
    pub fn unmarked(seq: u32, state: ImageState) -> Self {
        Self {
            marked: false,
            ..Self::new(seq, state)
        }
    }

    pub fn decode(raw: &[u8; ENTRY_LEN]) -> Self {
        let word = |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
        Self {
            seq: word(0),
            state: ImageState::from_raw(word(24)),
            crc: word(28),
            marked: raw[4..4 + MARKER.len()] == MARKER,
        }
    }

    pub fn encode(&self) -> [u8; ENTRY_LEN] {
        let mut raw = [0xFF; ENTRY_LEN];
        raw[0..4].copy_from_slice(&self.seq.to_le_bytes());
        if self.marked {
            raw[4..4 + MARKER.len()].copy_from_slice(&MARKER);
        }
        raw[24..28].copy_from_slice(&self.state.raw().to_le_bytes());
        raw[28..32].copy_from_slice(&self.crc.to_le_bytes());
        raw
    }

    /// Written completely: not erased and the CRC matches.
    pub fn is_intact(&self) -> bool {
        self.seq != ERASED_SEQ && self.crc == seq_crc(self.seq)
    }

    /// The bootloader would consider this record (`bootloader_common_ota_select_valid`).
    pub fn is_bootable(&self) -> bool {
        self.is_intact() && !matches!(self.state, ImageState::Invalid | ImageState::Aborted)
    }

    pub fn slot(&self) -> Slot {
        Slot::for_seq(self.seq)
    }
}

/// A record to write: erase `sector`, then write `entry` at its start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Update {
    pub sector: usize,
    pub entry: Entry,
}

/// Up to two updates, applied in order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    updates: [Option<Update>; 2],
}

impl Plan {
    pub fn updates(&self) -> impl Iterator<Item = Update> + '_ {
        self.updates.iter().flatten().copied()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OtaData {
    pub entries: [Entry; 2],
}

impl OtaData {
    pub const ERASED: OtaData = OtaData {
        entries: [Entry::ERASED; 2],
    };

    pub fn read<F: NorFlash>(flash: &mut F, partition: &Partition) -> Result<Self, FlashError> {
        let mut entries = [Entry::ERASED; 2];
        for (sector, entry) in entries.iter_mut().enumerate() {
            let mut raw = [0u8; ENTRY_LEN];
            partition.read(flash, sector as u32 * SECTOR, &mut raw)?;
            *entry = Entry::decode(&raw);
        }
        Ok(Self { entries })
    }

    /// The record the bootloader selects (`bootloader_common_select_otadata`).
    pub fn active(&self) -> Option<usize> {
        match (self.entries[0].is_bootable(), self.entries[1].is_bootable()) {
            (true, true) if self.entries[1].seq > self.entries[0].seq => Some(1),
            (true, _) => Some(0),
            (false, true) => Some(1),
            (false, false) => None,
        }
    }

    /// The slot the bootloader will try first. Without a bootable record and
    /// without a factory partition it starts `ota_0`.
    pub fn selected_slot(&self) -> Slot {
        self.active()
            .map_or(Slot::Ota0, |index| self.entries[index].slot())
    }

    /// State of the running image, when the active record describes it.
    pub fn state_of(&self, booted: Slot) -> Option<ImageState> {
        let index = self.active()?;
        let entry = self.entries[index];
        (entry.slot() == booted).then_some(entry.state)
    }

    fn max_seq(&self) -> u32 {
        self.entries
            .iter()
            .filter(|entry| entry.is_intact())
            .map(|entry| entry.seq)
            .max()
            .unwrap_or(0)
    }

    fn next_seq(&self, slot: Slot) -> u32 {
        let mut seq = self.max_seq() + 1;
        if Slot::for_seq(seq) != slot {
            seq += 1;
        }
        seq
    }

    /// The sector that may be overwritten without touching the active record.
    fn spare_sector(&self) -> usize {
        match self.active() {
            Some(index) => 1 - index,
            None => {
                let [a, b] = self.entries;
                if !a.is_intact() || (b.is_intact() && a.seq <= b.seq) {
                    0
                } else {
                    1
                }
            }
        }
    }

    fn push(&mut self, plan: &mut Plan, entry: Entry) {
        let sector = self.spare_sector();
        let update = Update { sector, entry };
        self.entries[sector] = entry;
        let free = plan.updates.iter_mut().find(|u| u.is_none());
        *free.expect("a plan holds at most two updates") = Some(update);
    }

    /// Selects `target` for the next boot as a `New` image.
    ///
    /// If the active record does not describe the running image (`booted`),
    /// a `Valid` record for it is written first so a rejected update falls
    /// back to exactly what is running now.
    pub fn plan_activate(&self, booted: Slot, target: Slot) -> Plan {
        let mut next = *self;
        let mut plan = Plan::default();
        if next.active().map(|index| next.entries[index].slot()) != Some(booted) {
            let seed = Entry::new(next.next_seq(booted), ImageState::Valid);
            next.push(&mut plan, seed);
        }
        let entry = Entry::new(next.next_seq(target), ImageState::New);
        next.push(&mut plan, entry);
        plan
    }

    /// Marks the running image `Valid` when it is still unconfirmed.
    pub fn plan_confirm(&self, booted: Slot) -> Plan {
        let mut next = *self;
        let mut plan = Plan::default();
        if next
            .state_of(booted)
            .is_some_and(ImageState::is_unconfirmed)
        {
            let entry = Entry::new(next.next_seq(booted), ImageState::Valid);
            next.push(&mut plan, entry);
        }
        plan
    }

    /// True on the first boot after a USB flash that erased `otadata`: the
    /// only record describing the running image is the one the bootloader
    /// wrote itself (`set_actual_ota_seq`), or there is none at all.
    pub fn is_fresh_usb_flash(&self, booted: Slot) -> bool {
        match self.active() {
            None => !self.entries.iter().any(Entry::is_intact),
            Some(index) => {
                let entry = self.entries[index];
                entry.slot() == booted && !entry.marked
            }
        }
    }

    /// Replaces a bootloader-written record for the running image with an
    /// equivalent marked one, so [`Self::is_fresh_usb_flash`] turns false.
    pub fn plan_mark(&self, booted: Slot) -> Plan {
        let mut next = *self;
        let mut plan = Plan::default();
        if next.is_fresh_usb_flash(booted) {
            let state = next.state_of(booted).unwrap_or(ImageState::Valid);
            let entry = Entry::new(next.next_seq(booted), state);
            next.push(&mut plan, entry);
        }
        plan
    }

    pub fn apply(&mut self, update: Update) {
        self.entries[update.sector] = update.entry;
    }
}

/// Writes a plan, verifying each record.
pub fn write_plan<F: NorFlash>(
    flash: &mut F,
    partition: &Partition,
    plan: &Plan,
) -> Result<(), FlashError> {
    if partition.len < OTADATA_LEN {
        return Err(FlashError::OutOfBounds);
    }
    for update in plan.updates() {
        let offset = update.sector as u32 * SECTOR;
        partition.erase_sector(flash, offset)?;
        partition.write_verified(flash, offset, &update.entry.encode())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flash::mock::MockFlash;

    // Records from esp-bootloader-esp-idf's tests, which mirror ESP-IDF.
    const SLOT_COUNT_1_VALID: [u8; 32] = [
        1, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        255, 255, 255, 255, 2, 0, 0, 0, 154, 152, 67, 71,
    ];
    const SLOT_COUNT_2_NEW: [u8; 32] = [
        2, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        255, 255, 255, 255, 0, 0, 0, 0, 116, 55, 246, 85,
    ];
    const SLOT_COUNT_3_PENDING: [u8; 32] = [
        3, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        255, 255, 255, 255, 1, 0, 0, 0, 17, 80, 74, 237,
    ];

    #[test]
    fn records_match_esp_idf_encoding() {
        assert_eq!(
            Entry::unmarked(1, ImageState::Valid).encode(),
            SLOT_COUNT_1_VALID
        );
        assert_eq!(
            Entry::unmarked(2, ImageState::New).encode(),
            SLOT_COUNT_2_NEW
        );
        assert_eq!(
            Entry::unmarked(3, ImageState::PendingVerify).encode(),
            SLOT_COUNT_3_PENDING
        );
        let ours = Entry::decode(&Entry::new(3, ImageState::PendingVerify).encode());
        assert!(ours.marked);
        assert_eq!(ours.seq, 3);
        let decoded = Entry::decode(&SLOT_COUNT_3_PENDING);
        assert_eq!(decoded, Entry::unmarked(3, ImageState::PendingVerify));
        assert!(decoded.is_bootable());
        assert_eq!(decoded.slot(), Slot::Ota0);
    }

    /// The ESP-IDF v5.5 selection logic with rollback enabled
    /// (`bootloader_utility_get_selected_boot_partition`). Returns the slot
    /// started; `None` means "no record, try ota_0".
    fn boot(data: &mut OtaData, rollback: bool) -> Slot {
        if rollback {
            for entry in &mut data.entries {
                if entry.state == ImageState::PendingVerify {
                    *entry = Entry {
                        state: ImageState::Aborted,
                        ..*entry
                    };
                }
            }
        }
        match data.active() {
            None => {
                // `set_actual_ota_seq`: with no intact record, record ota_0.
                if !data.entries.iter().any(Entry::is_intact) {
                    data.entries[0] = Entry::unmarked(1, ImageState::Valid);
                }
                Slot::Ota0
            }
            Some(index) => {
                if rollback && data.entries[index].state == ImageState::New {
                    data.entries[index].state = ImageState::PendingVerify;
                }
                data.entries[index].slot()
            }
        }
    }

    fn apply_plan(data: &mut OtaData, plan: Plan) {
        for update in plan.updates() {
            data.apply(update);
        }
    }

    #[test]
    fn erased_otadata_boots_ota0_and_update_keeps_exact_fallback() {
        let mut data = OtaData::ERASED;
        assert_eq!(data.state_of(Slot::Ota0), None);
        // Planned before the bootloader records anything: seeds ota_0 first.
        assert_eq!(
            data.plan_activate(Slot::Ota0, Slot::Ota1).updates().count(),
            2
        );

        assert_eq!(boot(&mut data, true), Slot::Ota0);
        assert_eq!(data.state_of(Slot::Ota0), Some(ImageState::Valid));
        let plan = data.plan_activate(Slot::Ota0, Slot::Ota1);
        assert_eq!(plan.updates().count(), 1);
        apply_plan(&mut data, plan);
        assert_eq!(data.selected_slot(), Slot::Ota1);

        // New image boots once, never confirms, resets: rolls back to ota_0.
        assert_eq!(boot(&mut data, true), Slot::Ota1);
        assert_eq!(data.state_of(Slot::Ota1), Some(ImageState::PendingVerify));
        assert_eq!(boot(&mut data, true), Slot::Ota0);
        assert_eq!(data.state_of(Slot::Ota0), Some(ImageState::Valid));
    }

    #[test]
    fn confirmed_update_sticks_and_next_update_alternates() {
        let mut data = OtaData::ERASED;
        let mut booted = boot(&mut data, true);
        for _ in 0..5 {
            let target = booted.other();
            let plan = data.plan_activate(booted, target);
            apply_plan(&mut data, plan);
            booted = boot(&mut data, true);
            assert_eq!(booted, target);
            assert_eq!(data.state_of(booted), Some(ImageState::PendingVerify));
            let confirm = data.plan_confirm(booted);
            assert_eq!(confirm.updates().count(), 1);
            apply_plan(&mut data, confirm);
            assert_eq!(data.state_of(booted), Some(ImageState::Valid));
            assert!(data.plan_confirm(booted).updates().next().is_none());
            // Reboots stay on the confirmed image.
            assert_eq!(boot(&mut data, true), booted);
            assert_eq!(boot(&mut data, true), booted);
        }
    }

    #[test]
    fn plans_never_overwrite_the_active_record() {
        let mut data = OtaData::ERASED;
        let mut booted = boot(&mut data, true);
        for round in 0..6 {
            let plan = data.plan_activate(booted, booted.other());
            let mut sim = data;
            for update in plan.updates() {
                assert_ne!(Some(update.sector), sim.active(), "round {round}");
                sim.apply(update);
            }
            data = sim;
            booted = boot(&mut data, true);
            if round % 2 == 0 {
                let confirm = data.plan_confirm(booted);
                for update in confirm.updates() {
                    assert_ne!(Some(update.sector), data.active());
                    data.apply(update);
                }
            } else {
                // Unconfirmed: next boot rolls back.
                booted = boot(&mut data, true);
            }
        }
    }

    #[test]
    fn power_loss_during_any_write_boots_a_known_image() {
        // Torn writes leave a record with a bad CRC.
        let torn = Entry {
            crc: 0,
            ..Entry::new(7, ImageState::New)
        };
        let mut data = OtaData::ERASED;
        let booted = boot(&mut data, true);
        let plan = data.plan_activate(booted, booted.other());
        for cut in 0..plan.updates().count() {
            let mut sim = data;
            for (i, update) in plan.updates().enumerate() {
                if i == cut {
                    sim.entries[update.sector] = torn;
                    break;
                }
                sim.apply(update);
            }
            assert_eq!(boot(&mut sim, true), booted, "cut at write {cut}");
        }
    }

    #[test]
    fn stale_pointer_to_the_other_slot_is_reseeded() {
        // Active record says ota_1 but ota_0 is running (ota_1 failed to load).
        let mut data = OtaData {
            entries: [
                Entry::new(1, ImageState::Valid),
                Entry::new(2, ImageState::Valid),
            ],
        };
        let plan = data.plan_activate(Slot::Ota0, Slot::Ota1);
        assert_eq!(plan.updates().count(), 2);
        apply_plan(&mut data, plan);
        assert_eq!(data.selected_slot(), Slot::Ota1);
        assert_eq!(boot(&mut data, true), Slot::Ota1);
        assert_eq!(boot(&mut data, true), Slot::Ota0, "rolls back to ota_0");
    }

    #[test]
    fn without_rollback_bootloader_new_stays_new() {
        let mut data = OtaData::ERASED;
        assert_eq!(boot(&mut data, false), Slot::Ota0);
        let plan = data.plan_activate(Slot::Ota0, Slot::Ota1);
        apply_plan(&mut data, plan);
        assert_eq!(boot(&mut data, false), Slot::Ota1);
        assert_eq!(data.state_of(Slot::Ota1), Some(ImageState::New));
        let plan = data.plan_confirm(Slot::Ota1);
        apply_plan(&mut data, plan);
        assert_eq!(data.state_of(Slot::Ota1), Some(ImageState::Valid));
    }

    #[test]
    fn fresh_usb_flash_is_detected_once() {
        // USB flash erased otadata; the bootloader records ota_0 itself.
        let mut data = OtaData::ERASED;
        assert!(data.is_fresh_usb_flash(Slot::Ota0));
        assert_eq!(boot(&mut data, true), Slot::Ota0);
        assert!(data.is_fresh_usb_flash(Slot::Ota0));

        let mark = data.plan_mark(Slot::Ota0);
        assert_eq!(mark.updates().count(), 1);
        apply_plan(&mut data, mark);
        assert!(!data.is_fresh_usb_flash(Slot::Ota0));
        assert_eq!(boot(&mut data, true), Slot::Ota0);
        assert!(!data.is_fresh_usb_flash(Slot::Ota0));
        assert!(data.plan_mark(Slot::Ota0).updates().next().is_none());

        // An OTA update and confirm never look like a USB flash.
        let plan = data.plan_activate(Slot::Ota0, Slot::Ota1);
        apply_plan(&mut data, plan);
        assert_eq!(boot(&mut data, true), Slot::Ota1);
        assert!(!data.is_fresh_usb_flash(Slot::Ota1));
        let plan = data.plan_confirm(Slot::Ota1);
        apply_plan(&mut data, plan);
        assert!(!data.is_fresh_usb_flash(Slot::Ota1));
    }

    #[test]
    fn write_plan_round_trips_through_flash() {
        let mut flash = MockFlash::new();
        let part = Partition {
            offset: 0x4000,
            len: OTADATA_LEN,
        };
        let data = OtaData::read(&mut flash, &part).unwrap();
        assert_eq!(data, OtaData::ERASED);
        let plan = data.plan_activate(Slot::Ota0, Slot::Ota1);
        write_plan(&mut flash, &part, &plan).unwrap();
        let mut expected = data;
        apply_plan(&mut expected, plan);
        assert_eq!(OtaData::read(&mut flash, &part).unwrap(), expected);
    }
}
