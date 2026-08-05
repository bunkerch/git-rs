//! Git object identifiers.

use std::fmt;
use std::str::FromStr;

use crate::Error;

/// A SHA-1 object identifier used by repository format version 0.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObjectId([u8; Self::LENGTH]);

impl ObjectId {
    pub const LENGTH: usize = 20;
    pub const HEX_LENGTH: usize = Self::LENGTH * 2;

    #[must_use]
    pub const fn from_bytes(bytes: [u8; Self::LENGTH]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; Self::LENGTH] {
        &self.0
    }

    #[must_use]
    pub fn is_null(&self) -> bool {
        self.0.iter().all(|byte| *byte == 0)
    }

    #[must_use]
    pub fn to_hex(self) -> [u8; Self::HEX_LENGTH] {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = [0; Self::HEX_LENGTH];
        for (index, byte) in self.0.iter().copied().enumerate() {
            output[index * 2] = HEX[usize::from(byte >> 4)];
            output[index * 2 + 1] = HEX[usize::from(byte & 0x0f)];
        }
        output
    }
}

impl FromStr for ObjectId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != Self::HEX_LENGTH {
            return Err(Error::InvalidObjectId(value.to_owned()));
        }
        let mut bytes = [0; Self::LENGTH];
        for (output, pair) in bytes.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
            let high =
                hex_value(pair[0]).ok_or_else(|| Error::InvalidObjectId(value.to_owned()))?;
            let low = hex_value(pair[1]).ok_or_else(|| Error::InvalidObjectId(value.to_owned()))?;
            *output = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = self.to_hex();
        // SAFETY is unnecessary: ASCII digits are valid UTF-8 by construction.
        let text = std::str::from_utf8(&hex).map_err(|_| fmt::Error)?;
        formatter.write_str(text)
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::ObjectId;

    #[test]
    fn parses_and_formats_git_hex_object_ids() {
        let text = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
        let id = ObjectId::from_str(text).unwrap();
        assert_eq!(id.to_string(), text);
        assert_eq!(id.as_bytes()[0..4], [0xe6, 0x9d, 0xe2, 0x9b]);
    }

    #[test]
    fn accepts_uppercase_input_and_canonicalizes_to_lowercase() {
        let id = ObjectId::from_str("E69DE29BB2D1D6434B8B29AE775AD8C2E48C5391").unwrap();
        assert_eq!(id.to_string(), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    }

    #[test]
    fn rejects_wrong_length_and_non_hex_input() {
        assert!(ObjectId::from_str("abc").is_err());
        assert!(ObjectId::from_str("g69de29bb2d1d6434b8b29ae775ad8c2e48c5391").is_err());
    }
}
