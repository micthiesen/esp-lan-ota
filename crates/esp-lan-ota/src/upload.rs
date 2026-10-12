//! One image upload into the spare slot.
//!
//! Chunks arrive in order at word-aligned offsets. A chunk that was already
//! written is checked against flash and accepted again, so a sender can resend
//! after a lost acknowledgement. Every write is read back, and [`Upload::finish`]
//! re-reads the whole image and compares its SHA-256 before anything changes
//! which slot boots.

use embedded_storage::nor_flash::NorFlash;
use sha2::{Digest, Sha256};

use crate::chip::Chip;
use crate::flash::{FlashError, Partition, SECTOR, WORD};
use crate::image::{ImageError, ImageInfo, HEADER_PREFIX_LEN, MIN_IMAGE_LEN};
use crate::name::Name;
use crate::otadata::Slot;

/// Largest chunk accepted. It fits a Matter invoke in one IPv6 packet.
pub const MAX_CHUNK: usize = 1024;

const READ_CHUNK: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadError {
    /// No upload has been started.
    NoUpload,
    /// The image does not fit the slot.
    TooLarge,
    /// The image is smaller than any real app image.
    TooSmall,
    /// Offset not word-aligned, or a non-final chunk length not a multiple of 4,
    /// or an empty or oversized chunk.
    BadChunk,
    /// The chunk does not continue the upload; resume from `expected`.
    OutOfOrder {
        expected: u32,
    },
    /// A resent chunk differs from what was already written.
    Conflict,
    /// The first chunk is not an ESP app image.
    BadImage(ImageError),
    /// The image is built for another chip.
    WrongChip,
    /// The image belongs to another project.
    WrongProject,
    /// `finish` before every byte arrived.
    Incomplete,
    /// The received or re-read image does not match the announced SHA-256.
    HashMismatch,
    /// The running image has not confirmed itself; updating now would
    /// overwrite the only known-good fallback.
    Unconfirmed,
    /// An update is staged; reboot before starting another.
    RebootPending,
    /// The app is not accepting uploads right now; retry shortly.
    Busy,
    Flash(FlashError),
}

impl From<FlashError> for UploadError {
    fn from(error: FlashError) -> Self {
        UploadError::Flash(error)
    }
}

/// What the running firmware accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Expect {
    pub chip: Chip,
    pub project: Name,
}

pub struct Upload {
    slot: Slot,
    partition: Partition,
    len: u32,
    sha256: [u8; 32],
    next: u32,
    hasher: Sha256,
    version: Name,
}

impl Upload {
    pub fn new(
        slot: Slot,
        partition: Partition,
        len: u32,
        sha256: [u8; 32],
    ) -> Result<Self, UploadError> {
        if len < MIN_IMAGE_LEN {
            return Err(UploadError::TooSmall);
        }
        // Leave room for the padding word after an unaligned end.
        if len.div_ceil(WORD) * WORD > partition.len {
            return Err(UploadError::TooLarge);
        }
        Ok(Self {
            slot,
            partition,
            len,
            sha256,
            next: 0,
            hasher: Sha256::new(),
            version: Name::EMPTY,
        })
    }

    pub fn slot(&self) -> Slot {
        self.slot
    }

    pub fn len(&self) -> u32 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes written so far; the offset of the next chunk.
    pub fn next(&self) -> u32 {
        self.next
    }

    /// The version from the image header, once the first chunk arrived.
    pub fn version(&self) -> Name {
        self.version
    }

    pub fn matches(&self, len: u32, sha256: &[u8; 32]) -> bool {
        self.len == len && &self.sha256 == sha256
    }

    pub fn write<F: NorFlash>(
        &mut self,
        flash: &mut F,
        expect: &Expect,
        offset: u32,
        data: &[u8],
    ) -> Result<(), UploadError> {
        let len = data.len() as u32;
        if data.is_empty() || data.len() > MAX_CHUNK || !offset.is_multiple_of(WORD) {
            return Err(UploadError::BadChunk);
        }
        let end = offset.checked_add(len).ok_or(UploadError::BadChunk)?;
        if end > self.len || (end != self.len && !len.is_multiple_of(WORD)) {
            return Err(UploadError::BadChunk);
        }
        if end <= self.next {
            return self.check_written(flash, offset, data);
        }
        if offset != self.next {
            return Err(UploadError::OutOfOrder {
                expected: self.next,
            });
        }
        if offset == 0 {
            self.version = Self::check_header(expect, data)?;
        }

        let mut sector = offset.next_multiple_of(SECTOR);
        while sector < end {
            self.partition.erase_sector(flash, sector)?;
            sector += SECTOR;
        }
        let aligned = data.len() & !(WORD as usize - 1);
        if aligned > 0 {
            self.partition
                .write_verified(flash, offset, &data[..aligned])?;
        }
        if aligned < data.len() {
            let mut word = [0xFF; WORD as usize];
            word[..data.len() - aligned].copy_from_slice(&data[aligned..]);
            self.partition
                .write_verified(flash, offset + aligned as u32, &word)?;
        }
        self.hasher.update(data);
        self.next = end;
        Ok(())
    }

    fn check_header(expect: &Expect, data: &[u8]) -> Result<Name, UploadError> {
        if data.len() < HEADER_PREFIX_LEN {
            return Err(UploadError::BadChunk);
        }
        let info = ImageInfo::parse(data).map_err(UploadError::BadImage)?;
        if info.chip != expect.chip {
            return Err(UploadError::WrongChip);
        }
        if info.project != expect.project {
            return Err(UploadError::WrongProject);
        }
        Ok(info.version)
    }

    fn check_written<F: NorFlash>(
        &self,
        flash: &mut F,
        offset: u32,
        data: &[u8],
    ) -> Result<(), UploadError> {
        let mut scratch = [0u8; READ_CHUNK];
        for (i, expected) in data.chunks(READ_CHUNK).enumerate() {
            let read_len = expected.len().next_multiple_of(WORD as usize);
            let read = &mut scratch[..read_len];
            self.partition
                .read(flash, offset + (i * READ_CHUNK) as u32, read)?;
            if &read[..expected.len()] != expected {
                return Err(UploadError::Conflict);
            }
        }
        Ok(())
    }

    /// Checks the received hash, then re-reads the slot and checks it again.
    pub fn finish<F: NorFlash>(&self, flash: &mut F) -> Result<(), UploadError> {
        if self.next != self.len {
            return Err(UploadError::Incomplete);
        }
        if self.hasher.clone().finalize().as_slice() != self.sha256 {
            return Err(UploadError::HashMismatch);
        }
        let mut hasher = Sha256::new();
        let mut scratch = [0u8; READ_CHUNK];
        let mut offset = 0;
        while offset < self.len {
            let take = (self.len - offset).min(READ_CHUNK as u32) as usize;
            let read = &mut scratch[..take.next_multiple_of(WORD as usize)];
            self.partition.read(flash, offset, read)?;
            hasher.update(&read[..take]);
            offset += take as u32;
        }
        if hasher.finalize().as_slice() != self.sha256 {
            return Err(UploadError::HashMismatch);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flash::mock::MockFlash;
    use crate::image::test_image;

    const SLOT: Partition = Partition {
        offset: 0x10000,
        len: 0x8000,
    };
    const EXPECT: Expect = Expect {
        chip: Chip::Esp32c3,
        project: match Name::new("demo") {
            Some(name) => name,
            None => panic!(),
        },
    };

    fn image(len: usize) -> ([u8; 8192], [u8; 32]) {
        let image = test_image(Chip::Esp32c3, "demo", "1.2.3", len);
        let sha: [u8; 32] = Sha256::digest(&image[..len]).into();
        (image, sha)
    }

    fn send(upload: &mut Upload, flash: &mut MockFlash, image: &[u8], chunk: usize) {
        let mut offset = 0;
        while offset < image.len() {
            let end = (offset + chunk).min(image.len());
            upload
                .write(flash, &EXPECT, offset as u32, &image[offset..end])
                .unwrap();
            offset = end;
        }
    }

    #[test]
    fn writes_verifies_and_hashes_an_unaligned_image() {
        let len = 8190;
        let (image, sha) = image(len);
        let mut flash = MockFlash::new();
        let mut upload = Upload::new(Slot::Ota1, SLOT, len as u32, sha).unwrap();
        send(&mut upload, &mut flash, &image[..len], 1000);
        upload.finish(&mut flash).unwrap();
        assert_eq!(upload.version().as_str(), "1.2.3");
        let start = SLOT.offset as usize;
        assert_eq!(&flash.data[start..start + len], &image[..len]);
        assert_eq!(&flash.data[start + len..start + len + 2], &[0xFF, 0xFF]);
        assert_eq!(flash.erases, 2);
    }

    #[test]
    fn resent_chunks_are_checked_not_rewritten() {
        let (image, sha) = image(8192);
        let mut flash = MockFlash::new();
        let mut upload = Upload::new(Slot::Ota0, SLOT, 8192, sha).unwrap();
        upload
            .write(&mut flash, &EXPECT, 0, &image[..1024])
            .unwrap();
        upload
            .write(&mut flash, &EXPECT, 1024, &image[1024..2048])
            .unwrap();
        // Lost acknowledgement: the sender repeats the last chunk.
        upload
            .write(&mut flash, &EXPECT, 1024, &image[1024..2048])
            .unwrap();
        let mut changed = [0u8; 1024];
        changed.copy_from_slice(&image[1024..2048]);
        changed[5] ^= 1;
        assert_eq!(
            upload.write(&mut flash, &EXPECT, 1024, &changed),
            Err(UploadError::Conflict)
        );
        assert_eq!(
            upload.write(&mut flash, &EXPECT, 4096, &image[4096..5120]),
            Err(UploadError::OutOfOrder { expected: 2048 })
        );
        send_from(&mut upload, &mut flash, &image, 2048);
        upload.finish(&mut flash).unwrap();
    }

    fn send_from(upload: &mut Upload, flash: &mut MockFlash, image: &[u8], from: usize) {
        let mut offset = from;
        while offset < image.len() {
            let end = (offset + 1024).min(image.len());
            upload
                .write(flash, &EXPECT, offset as u32, &image[offset..end])
                .unwrap();
            offset = end;
        }
    }

    #[test]
    fn rejects_foreign_or_malformed_images_before_writing() {
        let mut flash = MockFlash::new();
        let other = test_image(Chip::Esp32c6, "demo", "1", 4096);
        let mut upload = Upload::new(Slot::Ota1, SLOT, 4096, [0; 32]).unwrap();
        assert_eq!(
            upload.write(&mut flash, &EXPECT, 0, &other[..1024]),
            Err(UploadError::WrongChip)
        );
        let other = test_image(Chip::Esp32c3, "other", "1", 4096);
        assert_eq!(
            upload.write(&mut flash, &EXPECT, 0, &other[..1024]),
            Err(UploadError::WrongProject)
        );
        assert_eq!(
            upload.write(&mut flash, &EXPECT, 0, &[0u8; 1024]),
            Err(UploadError::BadImage(ImageError::BadMagic))
        );
        assert_eq!(flash.erases, 0);
        assert_eq!(upload.next(), 0);
    }

    #[test]
    fn rejects_bad_sizes_and_alignment() {
        assert_eq!(
            Upload::new(Slot::Ota1, SLOT, 100, [0; 32]).err(),
            Some(UploadError::TooSmall)
        );
        assert_eq!(
            Upload::new(Slot::Ota1, SLOT, SLOT.len + 1, [0; 32]).err(),
            Some(UploadError::TooLarge)
        );
        let (image, sha) = image(8192);
        let mut flash = MockFlash::new();
        let mut upload = Upload::new(Slot::Ota1, SLOT, 8192, sha).unwrap();
        assert_eq!(
            upload.write(&mut flash, &EXPECT, 0, &image[..1023]),
            Err(UploadError::BadChunk)
        );
        assert_eq!(
            upload.write(&mut flash, &EXPECT, 0, &image[..1025]),
            Err(UploadError::BadChunk)
        );
        assert_eq!(
            upload.write(&mut flash, &EXPECT, 2, &image[..4]),
            Err(UploadError::BadChunk)
        );
    }

    #[test]
    fn finish_requires_every_byte_and_the_right_hash() {
        let (image, sha) = image(8192);
        let mut flash = MockFlash::new();
        let mut upload = Upload::new(Slot::Ota1, SLOT, 8192, sha).unwrap();
        upload
            .write(&mut flash, &EXPECT, 0, &image[..1024])
            .unwrap();
        assert_eq!(upload.finish(&mut flash), Err(UploadError::Incomplete));

        let mut upload = Upload::new(Slot::Ota1, SLOT, 8192, [7; 32]).unwrap();
        send(&mut upload, &mut flash, &image[..8192], 1024);
        assert_eq!(upload.finish(&mut flash), Err(UploadError::HashMismatch));
    }

    #[test]
    fn finish_catches_flash_that_changed_after_writing() {
        let (image, sha) = image(8192);
        let mut flash = MockFlash::new();
        let mut upload = Upload::new(Slot::Ota1, SLOT, 8192, sha).unwrap();
        send(&mut upload, &mut flash, &image[..8192], 1024);
        flash.data[SLOT.offset as usize + 5000] ^= 0x10;
        assert_eq!(upload.finish(&mut flash), Err(UploadError::HashMismatch));
    }

    #[test]
    fn flash_write_failures_surface() {
        let (image, sha) = image(8192);
        let mut flash = MockFlash::new();
        flash.fail_after_bytes = Some(2048);
        let mut upload = Upload::new(Slot::Ota1, SLOT, 8192, sha).unwrap();
        upload
            .write(&mut flash, &EXPECT, 0, &image[..1024])
            .unwrap();
        upload
            .write(&mut flash, &EXPECT, 1024, &image[1024..2048])
            .unwrap();
        assert_eq!(
            upload.write(&mut flash, &EXPECT, 2048, &image[2048..3072]),
            Err(UploadError::Flash(FlashError::Write))
        );
        assert_eq!(upload.next(), 2048);
    }
}
