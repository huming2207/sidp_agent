//! Bounded SIDP framing, CRC-32/ISO-HDLC, and incremental USB SLIP decoding.
use std::io;

/// Largest decoded SIDP frame, including the 12-byte header.
pub const MAX_FRAME: usize = 8192;
/// SIDP header length in bytes.
pub const HEADER: usize = 12;
/// SIDP request message kind.
pub const REQUEST: u8 = 1;
/// SIDP response message kind.
pub const RESPONSE: u8 = 2;
/// SIDP asynchronous event kind.
pub const EVENT: u8 = 3;
/// SIDP low-priority log kind.
pub const LOG: u8 = 4;

/// A validated SIDP v1 frame. Payload structure is checked by the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Request, response, event, or log kind.
    pub kind: u8,
    /// Operation or event number.
    pub opcode: u16,
    /// Nonzero for requests/responses; zero for events/logs.
    pub id: u32,
    /// Payload excluding the wire header.
    pub payload: Vec<u8>,
}

pub(crate) fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// CRC-32/ISO-HDLC over a sequence of byte slices (the CRC field is excluded).
pub fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = !0u32;
    for byte in parts.iter().flat_map(|part| part.iter()) {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

impl Frame {
    fn check(&self) -> io::Result<()> {
        if !(REQUEST..=LOG).contains(&self.kind)
            || ((self.kind <= RESPONSE) != (self.id != 0))
            || self.payload.len() > MAX_FRAME - HEADER
        {
            return Err(invalid("invalid SIDP header or size"));
        }
        Ok(())
    }

    /// Encode a bounded frame, computing CRC over header bytes 0..8 and payload.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        self.check()?;
        let mut out = vec![1, self.kind];
        out.extend(self.opcode.to_le_bytes());
        out.extend(self.id.to_le_bytes());
        let crc = crc32(&[&out, &self.payload]);
        out.extend(crc.to_le_bytes());
        out.extend(&self.payload);
        Ok(out)
    }

    /// Decode one complete frame. Unknown versions and corrupt CRCs are errors.
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if !(HEADER..=MAX_FRAME).contains(&bytes.len()) || bytes[0] != 1 {
            return Err(invalid("invalid SIDP size or version"));
        }
        let mut r = Reader::new(&bytes[2..]);
        let opcode = r.u16()?;
        let id = r.u32()?;
        let crc = r.u32()?;
        if crc != crc32(&[&bytes[..8], &bytes[HEADER..]]) {
            return Err(invalid("SIDP CRC mismatch"));
        }
        let frame = Self {
            kind: bytes[1],
            opcode,
            id,
            payload: bytes[HEADER..].to_vec(),
        };
        frame.check()?;
        Ok(frame)
    }
}

/// SLIP-encode a complete SIDP frame with leading and trailing END delimiters.
pub fn slip_encode(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 2);
    out.push(0xc0);
    for &b in bytes {
        match b {
            0xc0 => out.extend([0xdb, 0xdc]),
            0xdb => out.extend([0xdb, 0xdd]),
            _ => out.push(b),
        }
    }
    out.push(0xc0);
    out
}

/// Incremental SLIP decoder with bounded storage and delimiter resynchronization.
///
/// Malformed/oversized packets are discarded through the next END. Empty packets
/// and noise before the first END are ignored. CRC validation belongs to `Frame`.
#[derive(Default)]
pub struct SlipDecoder {
    bytes: Vec<u8>,
    started: bool,
    escape: bool,
    discard: bool,
}

impl SlipDecoder {
    /// Consume a byte, returning a complete decoded packet at its END delimiter.
    pub fn push(&mut self, byte: u8) -> Option<Vec<u8>> {
        if byte == 0xc0 {
            let packet = if self.started && !self.discard && !self.escape && !self.bytes.is_empty()
            {
                Some(std::mem::take(&mut self.bytes))
            } else {
                None
            };
            self.bytes.clear();
            self.started = true;
            self.escape = false;
            self.discard = false;
            return packet;
        }
        if !self.started || self.discard {
            return None;
        }
        let value = if self.escape {
            self.escape = false;
            match byte {
                0xdc => 0xc0,
                0xdd => 0xdb,
                _ => {
                    self.discard = true;
                    return None;
                }
            }
        } else if byte == 0xdb {
            self.escape = true;
            return None;
        } else {
            byte
        };
        if self.bytes.len() == MAX_FRAME {
            self.discard = true;
        } else {
            self.bytes.push(value);
        }
        None
    }
}

/// Checked cursor for variable-length SIDP payloads.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
}
impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
    pub fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if n > self.bytes.len() {
            return Err(invalid("truncated SIDP payload"));
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Ok(head)
    }
    pub fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn end(self) -> io::Result<()> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(invalid("trailing SIDP payload bytes"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crc_and_frame_validation() {
        assert_eq!(crc32(&[b"123456789"]), 0xcbf43926);
        let frame = Frame {
            kind: REQUEST,
            opcode: 1,
            id: 1,
            payload: vec![0; 8],
        };
        let wire = frame.encode().unwrap();
        assert_eq!(Frame::decode(&wire).unwrap(), frame);
        for n in 0..wire.len() {
            assert!(Frame::decode(&wire[..n]).is_err());
        }
        let mut corrupt = wire;
        corrupt[12] ^= 1;
        assert!(Frame::decode(&corrupt).is_err());
        assert!(
            Frame {
                payload: vec![0; MAX_FRAME],
                ..frame
            }
            .encode()
            .is_err()
        );
    }
    #[test]
    fn slip_fragmentation_and_recovery() {
        let packet = [0xc0, 0xdb, 1, 2];
        let mut decoder = SlipDecoder::default();
        let out: Vec<_> = slip_encode(&packet)
            .into_iter()
            .filter_map(|b| decoder.push(b))
            .collect();
        assert_eq!(out, vec![packet.to_vec()]);
        for b in [0xc0, 0xdb, 0x01, 0x05, 0xc0] {
            assert!(decoder.push(b).is_none());
        }
        for _ in 0..MAX_FRAME + 2 {
            assert!(decoder.push(1).is_none());
        }
        assert!(decoder.push(0xc0).is_none());
        assert!(decoder.push(7).is_none());
        assert_eq!(decoder.push(0xc0), Some(vec![7]));
    }
}
