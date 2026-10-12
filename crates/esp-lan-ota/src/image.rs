//! Checks on the start of an ESP app image (the output of `espflash save-image`).
//!
//! Layout: a 24-byte image header (`0xE9` magic, `chip_id` at offset 12), an
//! 8-byte segment header, then `esp_app_desc_t` at offset 32 with the version
//! at 48 and the project name at 80.

use crate::chip::Chip;
use crate::name::{Name, NAME_LEN};

pub const IMAGE_MAGIC: u8 = 0xE9;
pub const APP_DESC_MAGIC: u32 = 0xABCD_5432;

const CHIP_ID_OFFSET: usize = 12;
const APP_DESC_OFFSET: usize = 32;
const VERSION_OFFSET: usize = APP_DESC_OFFSET + 16;
const PROJECT_OFFSET: usize = APP_DESC_OFFSET + 48;

/// Bytes needed to parse [`ImageInfo`].
pub const HEADER_PREFIX_LEN: usize = PROJECT_OFFSET + NAME_LEN;

/// Smallest image accepted. Real Rust ESP images are hundreds of KiB.
pub const MIN_IMAGE_LEN: u32 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    pub chip: Chip,
    pub version: Name,
    pub project: Name,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageError {
    TooShort,
    BadMagic,
    UnknownChip(u16),
    MissingAppDesc,
}

impl ImageInfo {
    pub fn parse(bytes: &[u8]) -> Result<Self, ImageError> {
        if bytes.len() < HEADER_PREFIX_LEN {
            return Err(ImageError::TooShort);
        }
        if bytes[0] != IMAGE_MAGIC {
            return Err(ImageError::BadMagic);
        }
        let chip_id = u16::from_le_bytes([bytes[CHIP_ID_OFFSET], bytes[CHIP_ID_OFFSET + 1]]);
        let chip = Chip::from_image_id(chip_id).ok_or(ImageError::UnknownChip(chip_id))?;
        let desc_magic = u32::from_le_bytes(
            bytes[APP_DESC_OFFSET..APP_DESC_OFFSET + 4]
                .try_into()
                .unwrap_or([0; 4]),
        );
        if desc_magic != APP_DESC_MAGIC {
            return Err(ImageError::MissingAppDesc);
        }
        Ok(Self {
            chip,
            version: Name::from_raw(field(bytes, VERSION_OFFSET)),
            project: Name::from_raw(field(bytes, PROJECT_OFFSET)),
        })
    }
}

fn field(bytes: &[u8], offset: usize) -> &[u8; NAME_LEN] {
    bytes[offset..offset + NAME_LEN]
        .try_into()
        .expect("length checked against HEADER_PREFIX_LEN")
}

/// Builds a minimal image prefix for tests.
#[cfg(test)]
pub(crate) fn test_image(chip: Chip, project: &str, version: &str, len: usize) -> [u8; 8192] {
    assert!(len <= 8192);
    let mut image = [0u8; 8192];
    image[0] = IMAGE_MAGIC;
    image[CHIP_ID_OFFSET..CHIP_ID_OFFSET + 2].copy_from_slice(&chip.image_id().to_le_bytes());
    image[APP_DESC_OFFSET..APP_DESC_OFFSET + 4].copy_from_slice(&APP_DESC_MAGIC.to_le_bytes());
    image[VERSION_OFFSET..VERSION_OFFSET + version.len()].copy_from_slice(version.as_bytes());
    image[PROJECT_OFFSET..PROJECT_OFFSET + project.len()].copy_from_slice(project.as_bytes());
    for (i, byte) in image.iter_mut().enumerate().skip(HEADER_PREFIX_LEN) {
        *byte = (i * 7 + 3) as u8;
    }
    image
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_key_right_header() {
        // First 112 bytes of key-right 0.1.4 from `espflash save-image`.
        let mut bytes = [0u8; HEADER_PREFIX_LEN];
        bytes[..16].copy_from_slice(&[
            0xe9, 0x05, 0x02, 0x20, 0x72, 0x00, 0x05, 0x42, 0xee, 0, 0, 0, 0x05, 0x00, 0x03, 0x00,
        ]);
        bytes[32..36].copy_from_slice(&[0x32, 0x54, 0xcd, 0xab]);
        bytes[48..53].copy_from_slice(b"0.1.4");
        bytes[80..93].copy_from_slice(b"key-right-app");
        let info = ImageInfo::parse(&bytes).unwrap();
        assert_eq!(info.chip, Chip::Esp32c3);
        assert_eq!(info.version.as_str(), "0.1.4");
        assert_eq!(info.project.as_str(), "key-right-app");
    }

    #[test]
    fn rejects_malformed_prefixes() {
        let good = test_image(Chip::Esp32c6, "p", "1", 4096);
        assert!(ImageInfo::parse(&good[..4096]).is_ok());
        assert_eq!(
            ImageInfo::parse(&good[..HEADER_PREFIX_LEN - 1]),
            Err(ImageError::TooShort)
        );
        let mut bad = good;
        bad[0] = 0;
        assert_eq!(ImageInfo::parse(&bad), Err(ImageError::BadMagic));
        let mut bad = good;
        bad[12] = 99;
        assert_eq!(ImageInfo::parse(&bad), Err(ImageError::UnknownChip(99)));
        let mut bad = good;
        bad[33] = 0;
        assert_eq!(ImageInfo::parse(&bad), Err(ImageError::MissingAppDesc));
    }
}
