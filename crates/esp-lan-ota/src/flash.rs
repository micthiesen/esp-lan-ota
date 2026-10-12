//! Bounded, verified access to one partition of a NOR flash.
//!
//! Every write is read back and compared. Offsets are relative to the
//! partition; the wrapped flash uses absolute addresses.

use embedded_storage::nor_flash::NorFlash;

/// Flash sector (erase unit) used by every supported chip.
pub const SECTOR: u32 = 4096;
/// Write and read granularity used by esp-storage.
pub const WORD: u32 = 4;

const COMPARE_CHUNK: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partition {
    pub offset: u32,
    pub len: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlashError {
    OutOfBounds,
    Misaligned,
    Erase,
    Write,
    Read,
    /// Data read back differs from what was written.
    Verify,
}

impl Partition {
    fn absolute(&self, offset: u32, len: u32) -> Result<u32, FlashError> {
        let end = offset.checked_add(len).ok_or(FlashError::OutOfBounds)?;
        if end > self.len {
            return Err(FlashError::OutOfBounds);
        }
        Ok(self.offset + offset)
    }

    /// Erases the sector starting at `offset`.
    pub fn erase_sector<F: NorFlash>(&self, flash: &mut F, offset: u32) -> Result<(), FlashError> {
        if !offset.is_multiple_of(SECTOR) {
            return Err(FlashError::Misaligned);
        }
        let start = self.absolute(offset, SECTOR)?;
        flash
            .erase(start, start + SECTOR)
            .map_err(|_| FlashError::Erase)
    }

    /// Writes word-aligned `data` and reads it back.
    pub fn write_verified<F: NorFlash>(
        &self,
        flash: &mut F,
        offset: u32,
        data: &[u8],
    ) -> Result<(), FlashError> {
        if !offset.is_multiple_of(WORD) || !(data.len() as u32).is_multiple_of(WORD) {
            return Err(FlashError::Misaligned);
        }
        let start = self.absolute(offset, data.len() as u32)?;
        flash.write(start, data).map_err(|_| FlashError::Write)?;
        let mut scratch = [0u8; COMPARE_CHUNK];
        for (i, expected) in data.chunks(COMPARE_CHUNK).enumerate() {
            let read = &mut scratch[..expected.len()];
            flash
                .read(start + (i * COMPARE_CHUNK) as u32, read)
                .map_err(|_| FlashError::Read)?;
            if read != expected {
                return Err(FlashError::Verify);
            }
        }
        Ok(())
    }

    /// Reads word-aligned data.
    pub fn read<F: NorFlash>(
        &self,
        flash: &mut F,
        offset: u32,
        out: &mut [u8],
    ) -> Result<(), FlashError> {
        if !offset.is_multiple_of(WORD) || !(out.len() as u32).is_multiple_of(WORD) {
            return Err(FlashError::Misaligned);
        }
        let start = self.absolute(offset, out.len() as u32)?;
        flash.read(start, out).map_err(|_| FlashError::Read)
    }
}

#[cfg(test)]
pub(crate) mod mock {
    //! NOR flash model: erase sets 0xFF, writes can only clear bits.
    use embedded_storage::nor_flash::{
        ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
    };

    pub const SIZE: usize = 0x20000;

    #[derive(Debug)]
    pub struct MockError;

    impl NorFlashError for MockError {
        fn kind(&self) -> NorFlashErrorKind {
            NorFlashErrorKind::Other
        }
    }

    pub struct MockFlash {
        pub data: [u8; SIZE],
        pub erases: usize,
        /// Bit errors injected into the next write, for verify tests.
        pub corrupt_next_write: bool,
        /// Fail writes once this many bytes have been written.
        pub fail_after_bytes: Option<usize>,
        written: usize,
    }

    impl MockFlash {
        pub fn new() -> Self {
            Self {
                data: [0xFF; SIZE],
                erases: 0,
                corrupt_next_write: false,
                fail_after_bytes: None,
                written: 0,
            }
        }
    }

    impl ErrorType for MockFlash {
        type Error = MockError;
    }

    impl ReadNorFlash for MockFlash {
        const READ_SIZE: usize = 4;
        fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), MockError> {
            assert_eq!(offset % 4, 0);
            assert_eq!(bytes.len() % 4, 0);
            let start = offset as usize;
            bytes.copy_from_slice(&self.data[start..start + bytes.len()]);
            Ok(())
        }
        fn capacity(&self) -> usize {
            SIZE
        }
    }

    impl NorFlash for MockFlash {
        const WRITE_SIZE: usize = 4;
        const ERASE_SIZE: usize = 4096;
        fn erase(&mut self, from: u32, to: u32) -> Result<(), MockError> {
            assert_eq!(from % 4096, 0);
            assert_eq!(to % 4096, 0);
            self.data[from as usize..to as usize].fill(0xFF);
            self.erases += 1;
            Ok(())
        }
        fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), MockError> {
            assert_eq!(offset % 4, 0);
            assert_eq!(bytes.len() % 4, 0);
            if let Some(limit) = self.fail_after_bytes {
                if self.written + bytes.len() > limit {
                    return Err(MockError);
                }
            }
            self.written += bytes.len();
            for (i, byte) in bytes.iter().enumerate() {
                self.data[offset as usize + i] &= byte;
            }
            if self.corrupt_next_write {
                self.corrupt_next_write = false;
                self.data[offset as usize] ^= 0x01;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockFlash;
    use super::*;

    const PART: Partition = Partition {
        offset: 0x2000,
        len: 0x2000,
    };

    #[test]
    fn writes_are_bounded_and_aligned() {
        let mut flash = MockFlash::new();
        assert_eq!(
            PART.write_verified(&mut flash, 0x1FFC, &[0; 8]),
            Err(FlashError::OutOfBounds)
        );
        assert_eq!(
            PART.write_verified(&mut flash, 2, &[0; 4]),
            Err(FlashError::Misaligned)
        );
        assert_eq!(
            PART.erase_sector(&mut flash, 0x100),
            Err(FlashError::Misaligned)
        );
        assert_eq!(
            PART.erase_sector(&mut flash, 0x2000),
            Err(FlashError::OutOfBounds)
        );
        PART.write_verified(&mut flash, 0x1FFC, &[1, 2, 3, 4])
            .unwrap();
        assert_eq!(&flash.data[0x3FFC..0x4000], &[1, 2, 3, 4]);
    }

    #[test]
    fn read_back_mismatch_is_reported() {
        let mut flash = MockFlash::new();
        flash.corrupt_next_write = true;
        assert_eq!(
            PART.write_verified(&mut flash, 0, &[0xF0; 128]),
            Err(FlashError::Verify)
        );
    }
}
