//! Transport-neutral primitives shared by Git's wire protocols.

use crate::{Error, Result};

/// Largest complete pkt-line accepted by Git.
pub const MAX_PACKET_LEN: usize = 65_520;
/// Largest data payload in one pkt-line.
pub const MAX_PACKET_DATA_LEN: usize = MAX_PACKET_LEN - 4;

/// A pkt-line data or control packet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PktLine {
    Data(Vec<u8>),
    Flush,
    Delimiter,
    ResponseEnd,
}

impl PktLine {
    /// Decode one packet from the beginning of `input`, returning bytes used.
    ///
    /// # Errors
    /// Returns an error for malformed, oversized, or incomplete framing.
    pub fn decode(input: &[u8]) -> Result<(Self, usize)> {
        match probe(input)? {
            Probe::Complete(packet, consumed) => Ok((packet, consumed)),
            Probe::Incomplete => protocol_error("truncated pkt-line"),
        }
    }

    /// Encode this packet in Git's four-hex-digit framing.
    ///
    /// # Errors
    /// Returns an error when a data packet exceeds Git's maximum packet size.
    pub fn encode(&self) -> Result<Vec<u8>> {
        match self {
            Self::Flush => Ok(b"0000".to_vec()),
            Self::Delimiter => Ok(b"0001".to_vec()),
            Self::ResponseEnd => Ok(b"0002".to_vec()),
            Self::Data(data) => {
                if data.len() > MAX_PACKET_DATA_LEN {
                    return protocol_error(format!(
                        "pkt-line payload is {} bytes; maximum is {MAX_PACKET_DATA_LEN}",
                        data.len()
                    ));
                }
                let length = data.len() + 4;
                let mut encoded = Vec::with_capacity(length);
                encoded.extend_from_slice(format!("{length:04x}").as_bytes());
                encoded.extend_from_slice(data);
                Ok(encoded)
            }
        }
    }
}

/// Incrementally decodes pkt-lines split across arbitrary transport chunks.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PktLineDecoder {
    buffer: Vec<u8>,
    cursor: usize,
}

impl PktLineDecoder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buffer: Vec::new(),
            cursor: 0,
        }
    }

    pub fn extend(&mut self, bytes: &[u8]) {
        self.compact();
        self.buffer.extend_from_slice(bytes);
    }

    /// Return the next complete packet, or `None` when more bytes are needed.
    ///
    /// # Errors
    /// Returns an error immediately for malformed or oversized framing.
    pub fn next_packet(&mut self) -> Result<Option<PktLine>> {
        match probe(&self.buffer[self.cursor..])? {
            Probe::Incomplete => Ok(None),
            Probe::Complete(packet, consumed) => {
                self.cursor += consumed;
                Ok(Some(packet))
            }
        }
    }

    /// Confirm that no truncated packet remains when the transport reaches EOF.
    ///
    /// # Errors
    /// Returns an error if unconsumed bytes remain.
    pub fn finish(mut self) -> Result<()> {
        self.compact();
        if self.buffer.is_empty() {
            Ok(())
        } else {
            protocol_error("truncated pkt-line at end of input")
        }
    }

    fn compact(&mut self) {
        if self.cursor != 0 {
            self.buffer.drain(..self.cursor);
            self.cursor = 0;
        }
    }
}

enum Probe {
    Incomplete,
    Complete(PktLine, usize),
}

fn probe(input: &[u8]) -> Result<Probe> {
    if input.len() < 4 {
        return Ok(Probe::Incomplete);
    }
    let length = parse_hex_length(&input[..4])?;
    match length {
        0 => Ok(Probe::Complete(PktLine::Flush, 4)),
        1 => Ok(Probe::Complete(PktLine::Delimiter, 4)),
        2 => Ok(Probe::Complete(PktLine::ResponseEnd, 4)),
        3 => protocol_error("reserved pkt-line length 0003"),
        4..=MAX_PACKET_LEN => {
            if input.len() < length {
                Ok(Probe::Incomplete)
            } else {
                Ok(Probe::Complete(
                    PktLine::Data(input[4..length].to_vec()),
                    length,
                ))
            }
        }
        _ => protocol_error(format!("pkt-line length {length} exceeds {MAX_PACKET_LEN}")),
    }
}

fn parse_hex_length(header: &[u8]) -> Result<usize> {
    let mut value = 0_usize;
    for byte in header {
        value = value
            .checked_mul(16)
            .and_then(|current| hex(*byte).map(|digit| current + usize::from(digit)))
            .ok_or_else(|| Error::Protocol("non-hex pkt-line length".into()))?;
    }
    Ok(value)
}

const fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// A decoded sideband payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Sideband {
    Data(Vec<u8>),
    Progress(Vec<u8>),
    Error(Vec<u8>),
}

impl Sideband {
    /// Decode the band byte and payload of a pkt-line data packet.
    ///
    /// # Errors
    /// Returns an error for a missing or unknown band designator.
    pub fn decode(data: &[u8]) -> Result<Self> {
        let (&band, payload) = data
            .split_first()
            .ok_or_else(|| Error::Protocol("empty sideband packet".into()))?;
        match band {
            1 => Ok(Self::Data(payload.to_vec())),
            2 => Ok(Self::Progress(payload.to_vec())),
            3 => Ok(Self::Error(payload.to_vec())),
            _ => protocol_error(format!("invalid sideband channel {band}")),
        }
    }

    /// Encode the band byte and payload as a pkt-line.
    ///
    /// # Errors
    /// Returns an error when the payload plus band byte exceeds pkt-line limits.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let (band, payload) = match self {
            Self::Data(payload) => (1, payload),
            Self::Progress(payload) => (2, payload),
            Self::Error(payload) => (3, payload),
        };
        let mut data = Vec::with_capacity(payload.len() + 1);
        data.push(band);
        data.extend_from_slice(payload);
        PktLine::Data(data).encode()
    }
}

/// A protocol capability in `name` or `name=value` form.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Capability {
    name: String,
    value: Option<String>,
}

impl Capability {
    /// Parse a space-separated capability list.
    ///
    /// # Errors
    /// Returns an error for non-ASCII, empty, duplicated, or control-containing
    /// capability tokens.
    pub fn parse_list(data: &[u8]) -> Result<Vec<Self>> {
        if !data.is_ascii() {
            return protocol_error("capabilities are not ASCII");
        }
        let text = std::str::from_utf8(data)
            .map_err(|_| Error::Protocol("capabilities are not UTF-8".into()))?;
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let mut capabilities = Vec::new();
        for token in text.split(' ') {
            if token.is_empty() || token.bytes().any(|byte| byte.is_ascii_control()) {
                return protocol_error("invalid capability token");
            }
            let (name, value) = token
                .split_once('=')
                .map_or((token, None), |(name, value)| (name, Some(value)));
            if name.is_empty() || capabilities.iter().any(|item: &Self| item.name == name) {
                return protocol_error(format!("empty or duplicate capability `{name}`"));
            }
            capabilities.push(Self {
                name: name.to_owned(),
                value: value.map(str::to_owned),
            });
        }
        Ok(capabilities)
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }
}

fn protocol_error<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Protocol(message.into()))
}

#[cfg(test)]
mod tests {
    use super::{Capability, MAX_PACKET_DATA_LEN, PktLine, PktLineDecoder, Sideband};

    #[test]
    fn pkt_lines_round_trip_data_and_control_packets() {
        for packet in [
            PktLine::Data(b"want abc\n".to_vec()),
            PktLine::Data(Vec::new()),
            PktLine::Flush,
            PktLine::Delimiter,
            PktLine::ResponseEnd,
        ] {
            let encoded = packet.encode().unwrap();
            let mut decoder = PktLineDecoder::new();
            for byte in encoded {
                decoder.extend(&[byte]);
            }
            assert_eq!(decoder.next_packet().unwrap(), Some(packet));
            decoder.finish().unwrap();
        }
    }

    #[test]
    fn decoder_retains_multiple_and_partial_packets() {
        let mut decoder = PktLineDecoder::new();
        decoder.extend(b"0008abcd0005x00");
        assert_eq!(
            decoder.next_packet().unwrap(),
            Some(PktLine::Data(b"abcd".to_vec()))
        );
        assert_eq!(
            decoder.next_packet().unwrap(),
            Some(PktLine::Data(vec![b'x']))
        );
        assert_eq!(decoder.next_packet().unwrap(), None);
        assert!(decoder.finish().is_err());
    }

    #[test]
    fn rejects_reserved_malformed_truncated_and_oversized_packets() {
        for invalid in [b"0003".as_slice(), b"zzzz", b"ffff"] {
            let mut decoder = PktLineDecoder::new();
            decoder.extend(invalid);
            assert!(decoder.next_packet().is_err());
        }
        assert!(
            PktLine::Data(vec![0; MAX_PACKET_DATA_LEN + 1])
                .encode()
                .is_err()
        );
    }

    #[test]
    fn sideband_round_trips_all_channels() {
        for band in [
            Sideband::Data(b"pack".to_vec()),
            Sideband::Progress(b"counting".to_vec()),
            Sideband::Error(b"failed".to_vec()),
        ] {
            let encoded = band.encode().unwrap();
            let mut decoder = PktLineDecoder::new();
            decoder.extend(&encoded);
            let Some(PktLine::Data(data)) = decoder.next_packet().unwrap() else {
                panic!("expected data packet");
            };
            assert_eq!(Sideband::decode(&data).unwrap(), band);
        }
        assert!(Sideband::decode(&[4]).is_err());
    }

    #[test]
    fn parses_capability_names_values_and_rejects_duplicates() {
        let capabilities = Capability::parse_list(
            b"multi_ack_detailed side-band-64k agent=git/2.53 object-format=sha1",
        )
        .unwrap();
        assert_eq!(capabilities[0].name(), "multi_ack_detailed");
        assert_eq!(capabilities[2].value(), Some("git/2.53"));
        assert!(Capability::parse_list(b"agent=a agent=b").is_err());
        assert!(Capability::parse_list(b"bad\nname").is_err());
    }

    #[test]
    fn rejects_pkt_line_with_oversized_length() {
        let mut decoder = PktLineDecoder::new();
        // FFFF hex = 65535 bytes - well above allowed max
        decoder.extend(b"\xFF\xFF\x00\x00");
        assert!(
            decoder.next_packet().is_err(),
            "oversized pkt-line should be rejected"
        );
    }

    #[test]
    fn pkt_line_decoder_accumulates_partial_data() {
        let mut decoder = PktLineDecoder::new();
        decoder.extend(b"00");
        assert!(
            decoder.next_packet().unwrap().is_none(),
            "partial header should produce None"
        );
        decoder.extend(b"05");
        assert!(
            decoder.next_packet().unwrap().is_none(),
            "partial body should produce None"
        );
        decoder.extend(b"a");
        let packet = decoder.next_packet().unwrap();
        assert_eq!(packet, Some(PktLine::Data(b"a".to_vec())));
    }
}
