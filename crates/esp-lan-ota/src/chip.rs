//! Supported chips and their ESP image `chip_id` values.

/// A chip esp-lan-ota supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Chip {
    Esp32c3,
    Esp32c6,
    Esp32h2,
    Esp32s3,
}

impl Chip {
    pub const ALL: [Chip; 4] = [Chip::Esp32c3, Chip::Esp32c6, Chip::Esp32h2, Chip::Esp32s3];

    /// The `esp_chip_id_t` value stored at offset 12 of an ESP app image.
    pub const fn image_id(self) -> u16 {
        match self {
            Chip::Esp32c3 => 5,
            Chip::Esp32s3 => 9,
            Chip::Esp32c6 => 13,
            Chip::Esp32h2 => 16,
        }
    }

    pub fn from_image_id(id: u16) -> Option<Self> {
        Self::ALL.into_iter().find(|chip| chip.image_id() == id)
    }

    /// The name espflash and esp-hal use, for example `esp32c3`.
    pub const fn name(self) -> &'static str {
        match self {
            Chip::Esp32c3 => "esp32c3",
            Chip::Esp32c6 => "esp32c6",
            Chip::Esp32h2 => "esp32h2",
            Chip::Esp32s3 => "esp32s3",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|chip| chip.name() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_names_round_trip() {
        for chip in Chip::ALL {
            assert_eq!(Chip::from_image_id(chip.image_id()), Some(chip));
            assert_eq!(Chip::from_name(chip.name()), Some(chip));
        }
        assert_eq!(Chip::from_image_id(0), None);
        assert_eq!(Chip::from_name("esp32"), None);
    }
}
