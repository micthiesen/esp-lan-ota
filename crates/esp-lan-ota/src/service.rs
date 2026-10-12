//! The device-side OTA state: boot report, self-confirmation and uploads.
//!
//! Transport-agnostic. The Matter cluster in [`crate::matter`] drives it, and
//! tests drive it directly.

use embedded_storage::nor_flash::NorFlash;

use crate::chip::Chip;
use crate::flash::FlashError;
use crate::name::Name;
use crate::otadata::{write_plan, ImageState, OtaData, Plan, Slot};
use crate::partition::{Layout, LayoutError};
use crate::upload::{Expect, Upload, UploadError};

/// What the running firmware is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub chip: Chip,
    /// `esp_app_desc_t.project_name`; images for other projects are refused.
    pub project: Name,
    pub version: Name,
}

/// Whether the bootloader rolls back unconfirmed images, as far as this boot
/// can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rollback {
    /// The image was already confirmed or came from USB; nothing observed.
    Unknown = 0,
    /// The bootloader moved this image from `New` to `PendingVerify`.
    Supported = 1,
    /// The image is still `New` after booting: the bootloader ignores states
    /// and will not roll back. Flash a rollback-enabled bootloader over USB.
    Unsupported = 2,
}

impl Rollback {
    pub const fn name(self) -> &'static str {
        match self {
            Rollback::Supported => "supported",
            Rollback::Unsupported => "unsupported",
            Rollback::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootInfo {
    pub booted: Slot,
    /// The OTA state of the running image when this boot started.
    pub state: Option<ImageState>,
    pub rollback: Rollback,
    /// This boot follows a USB flash and enrollment has not ended. The app
    /// opens a Matter commissioning window so `esp-ota enroll` can add its
    /// fabric; physical USB access is the consent. It ends after the window's
    /// 15 minutes or the first upload ([`Ota::end_enrollment`]); OTA boots
    /// never report it.
    pub enrollment_window: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartError {
    Layout(LayoutError),
    /// `booted_offset` is not `ota_0` or `ota_1`; the image was flashed to a
    /// factory partition or the table is not an OTA layout.
    NotOtaSlot,
    Flash(FlashError),
}

impl From<FlashError> for StartError {
    fn from(error: FlashError) -> Self {
        StartError::Flash(error)
    }
}

/// An update written, verified and selected for the next boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Staged {
    pub slot: Slot,
    pub version: Name,
}

/// A snapshot for status queries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub identity: Identity,
    pub booted: Slot,
    pub state: Option<ImageState>,
    pub rollback: Rollback,
    /// `(len, received)` of an upload in progress.
    pub upload: Option<(u32, u32)>,
    pub staged: Option<Staged>,
}

pub struct Ota<F> {
    flash: F,
    layout: Layout,
    identity: Identity,
    otadata: OtaData,
    boot: BootInfo,
    upload: Option<Upload>,
    staged: Option<Staged>,
    accepting: bool,
}

impl<F: NorFlash> Ota<F> {
    /// Reads the partition table and boot records.
    ///
    /// `booted_offset` is the flash offset of the running app partition, from
    /// `esp_bootloader_esp_idf::partitions::PartitionTable::booted_partition`.
    pub fn start(mut flash: F, booted_offset: u32, identity: Identity) -> Result<Self, StartError> {
        let layout = Layout::read(&mut flash).map_err(StartError::Layout)?;
        let booted = layout
            .slot_at(booted_offset)
            .ok_or(StartError::NotOtaSlot)?;
        let otadata = OtaData::read(&mut flash, &layout.otadata)?;
        let state = otadata.state_of(booted);
        let rollback = match state {
            Some(ImageState::PendingVerify) => Rollback::Supported,
            Some(ImageState::New) => Rollback::Unsupported,
            _ => Rollback::Unknown,
        };
        Ok(Self {
            flash,
            layout,
            identity,
            otadata,
            boot: BootInfo {
                booted,
                state,
                rollback,
                enrollment_window: otadata.is_fresh_usb_flash(booted),
            },
            upload: None,
            staged: None,
            accepting: true,
        })
    }

    pub fn boot_info(&self) -> BootInfo {
        self.boot
    }

    pub fn identity(&self) -> Identity {
        self.identity
    }

    /// Ends the after-USB-flash enrollment period, so later boots open no
    /// window. Called when the window has run its course and when the first
    /// upload starts. Until then a reset (crash, brownout, serial monitor)
    /// reopens the window, so enrollment survives one. Returns whether
    /// anything was written.
    pub fn end_enrollment(&mut self) -> Result<bool, FlashError> {
        let plan = self.otadata.plan_mark(self.boot.booted);
        let changed = plan.updates().next().is_some();
        self.write(plan)?;
        Ok(changed)
    }

    /// Marks the running image good. Call once the app's own health checks
    /// pass (for example Matter up and peripherals verified). Until then the
    /// bootloader rolls back on the next reset, and uploads are refused.
    /// Returns whether anything was written.
    pub fn confirm(&mut self) -> Result<bool, FlashError> {
        let plan = self.otadata.plan_confirm(self.boot.booted);
        let changed = plan.updates().next().is_some();
        self.write(plan)?;
        Ok(changed)
    }

    /// True while the running image still needs [`Self::confirm`].
    pub fn is_unconfirmed(&self) -> bool {
        self.otadata
            .state_of(self.boot.booted)
            .is_some_and(ImageState::is_unconfirmed)
    }

    /// Lets the app hold off new uploads (for example until a motor has
    /// stopped). `begin` reports [`UploadError::Busy`] while false; the host
    /// retries. An upload already in progress continues.
    pub fn set_accepting(&mut self, accepting: bool) {
        self.accepting = accepting;
    }

    /// True while an upload is in progress or staged.
    pub fn is_updating(&self) -> bool {
        self.upload.is_some() || self.staged.is_some()
    }

    /// Starts an upload, or resumes the one with the same length and hash.
    /// Returns the offset to send next.
    pub fn begin(&mut self, len: u32, sha256: &[u8; 32]) -> Result<u32, UploadError> {
        if self.staged.is_some() {
            return Err(UploadError::RebootPending);
        }
        if self.is_unconfirmed() {
            return Err(UploadError::Unconfirmed);
        }
        if let Some(upload) = &self.upload {
            if upload.matches(len, sha256) {
                return Ok(upload.next());
            }
        }
        if !self.accepting {
            return Err(UploadError::Busy);
        }
        let target = self.boot.booted.other();
        let upload = Upload::new(target, self.layout.slot(target), len, *sha256)?;
        // Before erasing the other slot, make sure the record the bootloader
        // follows describes the running image, not the slot being rewritten.
        self.write(self.otadata.plan_seed(self.boot.booted))?;
        self.end_enrollment()?;
        self.upload = Some(upload);
        Ok(0)
    }

    pub fn write_chunk(&mut self, offset: u32, data: &[u8]) -> Result<(), UploadError> {
        let expect = Expect {
            chip: self.identity.chip,
            project: self.identity.project,
        };
        let upload = self.upload.as_mut().ok_or(UploadError::NoUpload)?;
        let result = upload.write(&mut self.flash, &expect, offset, data);
        self.drop_if_unrecoverable(&result);
        result
    }

    /// Verifies the whole image and selects it for the next boot. The caller
    /// reboots afterwards.
    pub fn finish(&mut self) -> Result<Staged, UploadError> {
        let upload = self.upload.as_ref().ok_or(UploadError::NoUpload)?;
        let verified = upload.finish(&mut self.flash);
        self.drop_if_unrecoverable(&verified);
        verified?;
        let upload = self.upload.as_ref().ok_or(UploadError::NoUpload)?;
        let staged = Staged {
            slot: upload.slot(),
            version: upload.version(),
        };
        let plan = self.otadata.plan_activate(self.boot.booted, staged.slot);
        self.write(plan)?;
        if self.otadata.selected_slot() != staged.slot {
            return Err(UploadError::Flash(FlashError::Verify));
        }
        self.upload = None;
        self.staged = Some(staged);
        Ok(staged)
    }

    /// A flash error or a bad hash means the slot contents are unknown; the
    /// next `begin` must start over (and so erase again) instead of resuming.
    fn drop_if_unrecoverable<T>(&mut self, result: &Result<T, UploadError>) {
        if matches!(
            result,
            Err(UploadError::Flash(_) | UploadError::Conflict | UploadError::HashMismatch)
        ) {
            self.upload = None;
        }
    }

    /// Drops an upload in progress. Nothing that boots has changed.
    pub fn abort(&mut self) {
        self.upload = None;
    }

    pub fn reboot_pending(&self) -> bool {
        self.staged.is_some()
    }

    pub fn status(&self) -> Status {
        Status {
            identity: self.identity,
            booted: self.boot.booted,
            state: self.otadata.state_of(self.boot.booted),
            rollback: self.boot.rollback,
            upload: self.upload.as_ref().map(|u| (u.len(), u.next())),
            staged: self.staged,
        }
    }

    /// Writes a plan, then re-reads the records from flash whether or not
    /// the write succeeded, so later plans never start from a stale copy.
    fn write(&mut self, plan: Plan) -> Result<(), FlashError> {
        if plan.updates().next().is_none() {
            return Ok(());
        }
        let written = write_plan(&mut self.flash, &self.layout.otadata, &plan);
        self.otadata = OtaData::read(&mut self.flash, &self.layout.otadata)?;
        written
    }
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::flash::mock::MockFlash;
    use crate::image::test_image;
    use crate::otadata::Entry;
    use crate::partition::tests::write_test_table;

    const IDENTITY: Identity = Identity {
        chip: Chip::Esp32c6,
        project: match Name::new("demo") {
            Some(name) => name,
            None => panic!(),
        },
        version: match Name::new("1.0.0") {
            Some(name) => name,
            None => panic!(),
        },
    };

    /// Simulates a reboot through the ESP-IDF rollback bootloader.
    fn reboot(flash: &mut MockFlash, layout: &Layout) -> u32 {
        let mut data = OtaData::read(flash, &layout.otadata).unwrap();
        for entry in &mut data.entries {
            if entry.state == ImageState::PendingVerify {
                entry.state = ImageState::Aborted;
            }
        }
        let slot = match data.active() {
            None => {
                if !data.entries.iter().any(Entry::is_intact) {
                    data.entries[0] = Entry::unmarked(1, ImageState::Valid);
                }
                Slot::Ota0
            }
            Some(index) => {
                if data.entries[index].state == ImageState::New {
                    data.entries[index].state = ImageState::PendingVerify;
                }
                data.entries[index].slot()
            }
        };
        for (sector, entry) in data.entries.iter().enumerate() {
            let at = (layout.otadata.offset + sector as u32 * 4096) as usize;
            flash.data[at..at + 4096].fill(0xFF);
            if entry.seq != u32::MAX {
                flash.data[at..at + 32].copy_from_slice(&entry.encode());
            }
        }
        layout.slot(slot).offset
    }

    fn upload(ota: &mut Ota<&mut MockFlash>, version: &str) -> Staged {
        let image = test_image(Chip::Esp32c6, "demo", version, 8192);
        let sha: [u8; 32] = Sha256::digest(image).into();
        assert_eq!(ota.begin(8192, &sha).unwrap(), 0);
        for offset in (0..8192).step_by(1024) {
            ota.write_chunk(offset as u32, &image[offset..offset + 1024])
                .unwrap();
        }
        ota.finish().unwrap()
    }

    #[test]
    fn usb_flash_then_two_confirmed_updates() {
        let mut flash = MockFlash::new();
        let layout = write_test_table(&mut flash);

        // First boot after USB flash: enrollment window, which survives a
        // reset until it is ended.
        let booted = reboot(&mut flash, &layout);
        let ota = Ota::start(&mut flash, booted, IDENTITY).unwrap();
        assert!(ota.boot_info().enrollment_window);
        assert_eq!(ota.boot_info().booted, Slot::Ota0);
        let booted = reboot(&mut flash, &layout);
        let mut ota = Ota::start(&mut flash, booted, IDENTITY).unwrap();
        assert!(ota.boot_info().enrollment_window, "reopens after a reset");
        assert!(ota.end_enrollment().unwrap());
        assert!(!ota.end_enrollment().unwrap());
        let booted = reboot(&mut flash, &layout);
        let mut ota = Ota::start(&mut flash, booted, IDENTITY).unwrap();
        assert!(!ota.boot_info().enrollment_window);
        assert!(!ota.confirm().unwrap(), "USB image is already valid");

        let staged = upload(&mut ota, "2.0.0");
        assert_eq!(staged.slot, Slot::Ota1);
        assert_eq!(staged.version.as_str(), "2.0.0");
        assert!(ota.reboot_pending());
        assert_eq!(ota.begin(8192, &[0; 32]), Err(UploadError::RebootPending));

        let booted = reboot(&mut flash, &layout);
        let mut ota = Ota::start(&mut flash, booted, IDENTITY).unwrap();
        let boot = ota.boot_info();
        assert_eq!(boot.booted, Slot::Ota1);
        assert_eq!(boot.rollback, Rollback::Supported);
        assert!(!boot.enrollment_window);
        assert_eq!(ota.begin(8192, &[0; 32]), Err(UploadError::Unconfirmed));
        assert!(ota.confirm().unwrap());

        let staged = upload(&mut ota, "3.0.0");
        assert_eq!(staged.slot, Slot::Ota0);
        let booted = reboot(&mut flash, &layout);
        assert_eq!(booted, layout.slot(Slot::Ota0).offset);
    }

    #[test]
    fn unconfirmed_update_rolls_back() {
        let mut flash = MockFlash::new();
        let layout = write_test_table(&mut flash);
        let booted = reboot(&mut flash, &layout);
        let mut ota = Ota::start(&mut flash, booted, IDENTITY).unwrap();
        upload(&mut ota, "2.0.0");

        let booted = reboot(&mut flash, &layout);
        assert_eq!(booted, layout.slot(Slot::Ota1).offset);
        // New image never confirms; watchdog resets it.
        let booted = reboot(&mut flash, &layout);
        assert_eq!(booted, layout.slot(Slot::Ota0).offset);
        let ota = Ota::start(&mut flash, booted, IDENTITY).unwrap();
        assert!(!ota.boot_info().enrollment_window);
        assert!(!ota.is_unconfirmed());
    }

    #[test]
    fn interrupted_upload_resumes_and_a_new_hash_restarts() {
        let mut flash = MockFlash::new();
        let layout = write_test_table(&mut flash);
        let booted = reboot(&mut flash, &layout);
        let mut ota = Ota::start(&mut flash, booted, IDENTITY).unwrap();
        let image = test_image(Chip::Esp32c6, "demo", "2", 8192);
        let sha: [u8; 32] = Sha256::digest(image).into();
        ota.begin(8192, &sha).unwrap();
        ota.write_chunk(0, &image[..1024]).unwrap();
        ota.write_chunk(1024, &image[1024..2048]).unwrap();
        assert_eq!(ota.begin(8192, &sha).unwrap(), 2048);
        assert_eq!(ota.status().upload, Some((8192, 2048)));
        assert_eq!(ota.begin(8192, &[1; 32]).unwrap(), 0);
        ota.abort();
        assert_eq!(
            ota.write_chunk(0, &image[..1024]),
            Err(UploadError::NoUpload)
        );
    }

    #[test]
    fn first_upload_ends_enrollment() {
        let mut flash = MockFlash::new();
        let layout = write_test_table(&mut flash);
        let booted = reboot(&mut flash, &layout);
        let mut ota = Ota::start(&mut flash, booted, IDENTITY).unwrap();
        assert!(ota.boot_info().enrollment_window);
        ota.begin(8192, &[0; 32]).unwrap();
        let booted = reboot(&mut flash, &layout);
        let ota = Ota::start(&mut flash, booted, IDENTITY).unwrap();
        assert!(!ota.boot_info().enrollment_window);
    }

    #[test]
    fn busy_gate_holds_new_uploads_but_not_resumes() {
        let mut flash = MockFlash::new();
        let layout = write_test_table(&mut flash);
        let booted = reboot(&mut flash, &layout);
        let mut ota = Ota::start(&mut flash, booted, IDENTITY).unwrap();
        ota.set_accepting(false);
        assert_eq!(ota.begin(8192, &[1; 32]), Err(UploadError::Busy));
        assert!(!ota.is_updating());
        ota.set_accepting(true);
        assert_eq!(ota.begin(8192, &[1; 32]), Ok(0));
        ota.set_accepting(false);
        assert_eq!(ota.begin(8192, &[1; 32]), Ok(0), "resume is allowed");
        assert!(ota.is_updating());
    }

    #[test]
    fn unrecoverable_errors_restart_the_upload() {
        let mut flash = MockFlash::new();
        let layout = write_test_table(&mut flash);
        let booted = reboot(&mut flash, &layout);
        let mut ota = Ota::start(&mut flash, booted, IDENTITY).unwrap();
        let image = test_image(Chip::Esp32c6, "demo", "2", 8192);

        // Announced hash wrong: finish fails and the next begin starts at 0.
        ota.begin(8192, &[9; 32]).unwrap();
        for offset in (0..8192).step_by(1024) {
            ota.write_chunk(offset as u32, &image[offset..offset + 1024])
                .unwrap();
        }
        assert_eq!(ota.finish(), Err(UploadError::HashMismatch));
        assert_eq!(ota.begin(8192, &[9; 32]).unwrap(), 0);

        // A resend that differs from flash also restarts.
        let sha: [u8; 32] = Sha256::digest(image).into();
        ota.begin(8192, &sha).unwrap();
        ota.write_chunk(0, &image[..1024]).unwrap();
        let mut changed = [0u8; 1024];
        changed.copy_from_slice(&image[..1024]);
        changed[200] ^= 1;
        assert_eq!(ota.write_chunk(0, &changed), Err(UploadError::Conflict));
        assert_eq!(ota.begin(8192, &sha).unwrap(), 0);
    }

    #[test]
    fn failed_activation_never_rewrites_the_active_record() {
        let mut flash = MockFlash::new();
        let layout = write_test_table(&mut flash);
        // Erased otadata: activation needs a seed write and a New write.
        let mut ota = Ota::start(&mut flash, layout.slot(Slot::Ota0).offset, IDENTITY).unwrap();
        let image = test_image(Chip::Esp32c6, "demo", "2", 8192);
        let sha: [u8; 32] = Sha256::digest(image).into();
        ota.begin(8192, &sha).unwrap();
        for offset in (0..8192).step_by(1024) {
            ota.write_chunk(offset as u32, &image[offset..offset + 1024])
                .unwrap();
        }
        // begin() already seeded ota_0; fail the New record.
        let active_before = OtaData::read(&mut *ota_flash(&mut ota), &layout.otadata)
            .unwrap()
            .active()
            .unwrap();
        ota_flash(&mut ota).fail_after_bytes = Some(0);
        assert!(matches!(ota.finish(), Err(UploadError::Flash(_))));
        ota_flash(&mut ota).fail_after_bytes = None;
        let data = OtaData::read(&mut *ota_flash(&mut ota), &layout.otadata).unwrap();
        assert_eq!(data.active(), Some(active_before));
        assert_eq!(data.selected_slot(), Slot::Ota0);
    }

    fn ota_flash<'a>(ota: &'a mut Ota<&mut MockFlash>) -> &'a mut MockFlash {
        &mut *ota.flash
    }

    #[test]
    fn factory_layout_is_refused() {
        let mut flash = MockFlash::new();
        let layout = write_test_table(&mut flash);
        assert_eq!(
            Ota::start(&mut flash, layout.otadata.offset, IDENTITY).err(),
            Some(StartError::NotOtaSlot)
        );
    }
}
