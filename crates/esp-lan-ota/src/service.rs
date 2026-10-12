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
    /// This is the first boot after a USB flash. The app opens a Matter
    /// commissioning window so `esp-ota enroll` can add its fabric; physical
    /// USB access is the consent. Later boots, including OTA updates, report
    /// `false`.
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
}

impl<F: NorFlash> Ota<F> {
    /// Reads the partition table and boot records, and consumes the
    /// first-boot-after-USB marker.
    ///
    /// `booted_offset` is the flash offset of the running app partition, from
    /// `esp_bootloader_esp_idf::partitions::PartitionTable::booted_partition`.
    pub fn start(mut flash: F, booted_offset: u32, identity: Identity) -> Result<Self, StartError> {
        let layout = Layout::read(&mut flash).map_err(StartError::Layout)?;
        let booted = layout
            .slot_at(booted_offset)
            .ok_or(StartError::NotOtaSlot)?;
        let mut otadata = OtaData::read(&mut flash, &layout.otadata)?;
        let state = otadata.state_of(booted);
        let rollback = match state {
            Some(ImageState::PendingVerify) => Rollback::Supported,
            Some(ImageState::New) => Rollback::Unsupported,
            _ => Rollback::Unknown,
        };
        let enrollment_window = otadata.is_fresh_usb_flash(booted);
        let mut ota = Self {
            flash,
            layout,
            identity,
            otadata,
            boot: BootInfo {
                booted,
                state,
                rollback,
                enrollment_window,
            },
            upload: None,
            staged: None,
        };
        if enrollment_window {
            ota.write(otadata.plan_mark(booted))?;
            otadata = ota.otadata;
            debug_assert!(!otadata.is_fresh_usb_flash(booted));
        }
        Ok(ota)
    }

    pub fn boot_info(&self) -> BootInfo {
        self.boot
    }

    pub fn identity(&self) -> Identity {
        self.identity
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
        let target = self.boot.booted.other();
        self.upload = Some(Upload::new(target, self.layout.slot(target), len, *sha256)?);
        Ok(0)
    }

    pub fn write_chunk(&mut self, offset: u32, data: &[u8]) -> Result<(), UploadError> {
        let expect = Expect {
            chip: self.identity.chip,
            project: self.identity.project,
        };
        let upload = self.upload.as_mut().ok_or(UploadError::NoUpload)?;
        upload.write(&mut self.flash, &expect, offset, data)
    }

    /// Verifies the whole image and selects it for the next boot. The caller
    /// reboots afterwards.
    pub fn finish(&mut self) -> Result<Staged, UploadError> {
        let upload = self.upload.as_ref().ok_or(UploadError::NoUpload)?;
        upload.finish(&mut self.flash)?;
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

    /// Writes a plan and re-reads the records from flash.
    fn write(&mut self, plan: Plan) -> Result<(), FlashError> {
        if plan.updates().next().is_none() {
            return Ok(());
        }
        write_plan(&mut self.flash, &self.layout.otadata, &plan)?;
        self.otadata = OtaData::read(&mut self.flash, &self.layout.otadata)?;
        Ok(())
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

        // First boot after USB flash: enrollment window, marked once.
        let booted = reboot(&mut flash, &layout);
        let ota = Ota::start(&mut flash, booted, IDENTITY).unwrap();
        assert!(ota.boot_info().enrollment_window);
        assert_eq!(ota.boot_info().booted, Slot::Ota0);
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
    fn factory_layout_is_refused() {
        let mut flash = MockFlash::new();
        let layout = write_test_table(&mut flash);
        assert_eq!(
            Ota::start(&mut flash, layout.otadata.offset, IDENTITY).err(),
            Some(StartError::NotOtaSlot)
        );
    }
}
