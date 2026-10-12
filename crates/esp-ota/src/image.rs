//! Turns an ELF (or an existing app image) into the bytes to upload.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use esp_lan_ota::image::{ImageInfo, HEADER_PREFIX_LEN};
use esp_lan_ota::Chip;
use sha2::{Digest, Sha256};

pub struct Image {
    pub bytes: Vec<u8>,
    pub sha256: [u8; 32],
    pub info: ImageInfo,
}

/// Reads `path`. A file starting with the ESP image magic is used as is;
/// anything else is treated as an ELF and converted with
/// `espflash save-image --chip <chip>`.
pub fn load(path: &Path, chip: Chip, espflash_args: &[String]) -> Result<Image> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let bytes = if raw.first() == Some(&esp_lan_ota::image::IMAGE_MAGIC) {
        raw
    } else if raw.starts_with(b"\x7fELF") {
        let dir = tempdir()?;
        let out = dir.join("image.bin");
        let status = Command::new("espflash")
            .arg("save-image")
            .arg("--chip")
            .arg(chip.name())
            .args(espflash_args)
            .arg(path)
            .arg(&out)
            .status()
            .context("running espflash save-image (is espflash installed?)")?;
        if !status.success() {
            bail!("espflash save-image failed");
        }
        let bytes = std::fs::read(&out)?;
        let _ = std::fs::remove_dir_all(&dir);
        bytes
    } else {
        bail!("{} is neither an ELF nor an ESP app image", path.display());
    };
    if bytes.len() < HEADER_PREFIX_LEN {
        bail!("image too small");
    }
    let info = ImageInfo::parse(&bytes).map_err(|e| anyhow::anyhow!("not an ESP app image: {e:?}"))?;
    let sha256 = Sha256::digest(&bytes).into();
    Ok(Image {
        bytes,
        sha256,
        info,
    })
}

fn tempdir() -> Result<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!("esp-ota-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
