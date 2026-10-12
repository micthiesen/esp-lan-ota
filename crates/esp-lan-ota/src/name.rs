//! Fixed 32-byte, NUL-padded text fields as stored in `esp_app_desc_t`.

use core::fmt;

pub const NAME_LEN: usize = 32;

/// A project name or version string, at most 32 bytes, NUL padded.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Name([u8; NAME_LEN]);

impl Name {
    pub const EMPTY: Name = Name([0; NAME_LEN]);

    /// Builds a name, rejecting text longer than 32 bytes or containing NUL.
    pub const fn new(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() > NAME_LEN {
            return None;
        }
        let mut raw = [0; NAME_LEN];
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0 {
                return None;
            }
            raw[i] = bytes[i];
            i += 1;
        }
        Some(Self(raw))
    }

    /// Reads a raw field. Everything after the first NUL is ignored so names
    /// compare equal regardless of what the image left in the padding.
    pub fn from_raw(raw: &[u8; NAME_LEN]) -> Self {
        let len = raw.iter().position(|&b| b == 0).unwrap_or(NAME_LEN);
        let mut clean = [0; NAME_LEN];
        clean[..len].copy_from_slice(&raw[..len]);
        Self(clean)
    }

    pub fn as_bytes(&self) -> &[u8; NAME_LEN] {
        &self.0
    }

    /// The text up to the first NUL, or `"?"` when it is not UTF-8.
    pub fn as_str(&self) -> &str {
        let len = self.0.iter().position(|&b| b == 0).unwrap_or(NAME_LEN);
        core::str::from_utf8(&self.0[..len]).unwrap_or("?")
    }

    pub fn is_empty(&self) -> bool {
        self.0[0] == 0
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_after_nul_is_ignored() {
        let mut raw = [0xAA; NAME_LEN];
        raw[..3].copy_from_slice(b"abc");
        raw[3] = 0;
        assert_eq!(Name::from_raw(&raw), Name::new("abc").unwrap());
    }

    #[test]
    fn rejects_long_or_nul_text() {
        assert!(Name::new(&"x".repeat(33)).is_none());
        assert!(Name::new("a\0b").is_none());
        assert_eq!(Name::new(&"x".repeat(32)).unwrap().as_str().len(), 32);
    }
}
