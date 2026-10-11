//! RTP packets (RFC 3550) carrying linear PCM as AES67 uses it (RFC 3190).

/// Length of an RTP header without CSRCs or extension.
pub const HEADER_LEN: usize = 12;

/// The fixed RTP header fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Shorter than its header, extension or padding claims.
    Truncated,
    /// Not RTP version 2.
    Version,
}

impl Header {
    /// Writes the header without CSRCs or extension.
    pub fn write(&self, buf: &mut [u8; HEADER_LEN]) {
        buf[0] = 2 << 6;
        buf[1] = (self.marker as u8) << 7 | (self.payload_type & 0x7f);
        buf[2..4].copy_from_slice(&self.sequence.to_be_bytes());
        buf[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        buf[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
    }
}

/// Splits a packet into its header and payload, skipping CSRCs, header
/// extension and padding.
pub fn parse(packet: &[u8]) -> Result<(Header, &[u8]), ParseError> {
    if packet.len() < HEADER_LEN {
        return Err(ParseError::Truncated);
    }
    let b0 = packet[0];
    if b0 >> 6 != 2 {
        return Err(ParseError::Version);
    }
    let be16 = |i: usize| u16::from_be_bytes([packet[i], packet[i + 1]]);
    let be32 =
        |i: usize| u32::from_be_bytes([packet[i], packet[i + 1], packet[i + 2], packet[i + 3]]);
    let header = Header {
        marker: packet[1] & 0x80 != 0,
        payload_type: packet[1] & 0x7f,
        sequence: be16(2),
        timestamp: be32(4),
        ssrc: be32(8),
    };

    let mut start = HEADER_LEN + 4 * (b0 & 0x0f) as usize;
    if b0 & 0x10 != 0 {
        if packet.len() < start + 4 {
            return Err(ParseError::Truncated);
        }
        start += 4 + 4 * be16(start + 2) as usize;
    }
    let mut end = packet.len();
    if b0 & 0x20 != 0 {
        let pad = packet[end - 1] as usize;
        if pad == 0 || pad > end {
            return Err(ParseError::Truncated);
        }
        end -= pad;
    }
    if start > end {
        return Err(ParseError::Truncated);
    }
    Ok((header, &packet[start..end]))
}

/// Big-endian linear PCM sample encodings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoding {
    L16,
    L24,
}

impl Encoding {
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::L16 => 2,
            Self::L24 => 3,
        }
    }

    /// Encodes `samples` (full scale ±1.0, clipped beyond) into `out` and
    /// returns the number of bytes written. Stops early if `out` is too short.
    pub fn encode(self, samples: &[f32], out: &mut [u8]) -> usize {
        let bps = self.bytes_per_sample();
        let n = samples.len().min(out.len() / bps);
        let bits = 8 * bps as u32;
        for (s, o) in samples[..n].iter().zip(out.chunks_exact_mut(bps)) {
            let v = quantize(*s, bits).to_be_bytes();
            o.copy_from_slice(&v[4 - bps..]);
        }
        n * bps
    }

    /// Decodes `payload` into `out` and returns the number of samples
    /// written. A trailing partial sample is ignored.
    pub fn decode(self, payload: &[u8], out: &mut [f32]) -> usize {
        let bps = self.bytes_per_sample();
        let scale = 1.0 / (1u32 << (8 * bps - 1)) as f32;
        let mut n = 0;
        for (b, o) in payload.chunks_exact(bps).zip(out.iter_mut()) {
            let mut v = [0u8; 4];
            v[..bps].copy_from_slice(b);
            // Left-aligned, so the arithmetic shift sign-extends.
            let v = i32::from_be_bytes(v) >> (8 * (4 - bps));
            *o = v as f32 * scale;
            n += 1;
        }
        n
    }
}

/// Rounds `s * 2^(bits-1)` to the nearest integer that fits `bits` bits.
fn quantize(s: f32, bits: u32) -> i32 {
    let max = (1i32 << (bits - 1)) - 1;
    let x = s * (max + 1) as f32;
    // `as` saturates and maps NaN to 0.
    let v = if x >= 0.0 { x + 0.5 } else { x - 0.5 } as i32;
    v.clamp(-max - 1, max)
}

/// Counts lost packets from sequence numbers the way RFC 3550 does:
/// expected (highest - lowest + 1) minus received. Duplicates make up for
/// losses, which is fine for monitoring.
#[derive(Debug, Clone, Copy, Default)]
pub struct SequenceStats {
    /// Lowest and highest extended sequence number seen; `None` before the
    /// first packet.
    range: Option<(i64, i64)>,
    received: u64,
}

impl SequenceStats {
    pub fn record(&mut self, sequence: u16) {
        self.received += 1;
        let (lo, hi) = match self.range {
            None => (sequence as i64, sequence as i64),
            Some((lo, hi)) => {
                let ext = hi + sequence.wrapping_sub(hi as u16) as i16 as i64;
                (lo.min(ext), hi.max(ext))
            }
        };
        self.range = Some((lo, hi));
    }

    pub fn received(&self) -> u64 {
        self.received
    }

    pub fn lost(&self) -> u64 {
        let expected = self.range.map_or(0, |(lo, hi)| (hi - lo + 1) as u64);
        expected.saturating_sub(self.received)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let h = Header {
            marker: true,
            payload_type: 97,
            sequence: 0xfffe,
            timestamp: 0x8000_0001,
            ssrc: 0xdead_beef,
        };
        let mut packet = [0u8; HEADER_LEN + 6];
        h.write((&mut packet[..HEADER_LEN]).try_into().unwrap());
        packet[HEADER_LEN..].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        let (parsed, payload) = parse(&packet).unwrap();
        assert_eq!(parsed, h);
        assert_eq!(payload, &[1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn parse_skips_csrc_extension_and_padding() {
        let mut p = [0u8; 12 + 4 + 8 + 3 + 2];
        p[0] = 2 << 6 | 0x20 | 0x10 | 1; // padding, extension, 1 CSRC
        p[16 + 2..16 + 4].copy_from_slice(&1u16.to_be_bytes()); // 1 ext word
        p[24..27].copy_from_slice(&[7, 8, 9]);
        p[28] = 2; // 2 bytes padding
        let (_, payload) = parse(&p).unwrap();
        assert_eq!(payload, &[7, 8, 9]);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert_eq!(parse(&[0x80; 11]), Err(ParseError::Truncated));
        assert_eq!(parse(&[0x40; 12]), Err(ParseError::Version));
        let mut p = [0u8; 12];
        p[0] = 2 << 6 | 3; // 3 CSRCs missing
        assert_eq!(parse(&p), Err(ParseError::Truncated));
        p[0] = 2 << 6 | 0x20; // padding byte 0
        assert_eq!(parse(&p), Err(ParseError::Truncated));
    }

    #[test]
    fn l24_is_big_endian_and_clips() {
        let mut out = [0u8; 12];
        Encoding::L24.encode(&[0.5, -1.0, 2.0, -0.000_000_119_209_29], &mut out);
        assert_eq!(
            out,
            [0x40, 0, 0, 0x80, 0, 0, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff]
        );
        let mut back = [0.0; 4];
        assert_eq!(Encoding::L24.decode(&out, &mut back), 4);
        assert_eq!(back[..2], [0.5, -1.0]);
        assert_eq!(back[3], -1.0 / 8_388_608.0);
    }

    #[test]
    fn integer_samples_survive_round_trip() {
        for enc in [Encoding::L16, Encoding::L24] {
            let bits = 8 * enc.bytes_per_sample() as u32;
            let full = (1i32 << (bits - 1)) as f32;
            let ints = [
                -(1 << (bits - 1)),
                -12345,
                -1,
                0,
                1,
                777,
                (1 << (bits - 1)) - 1,
            ];
            let samples = ints.map(|i| i as f32 / full);
            let mut bytes = [0u8; 21];
            let n = enc.encode(&samples, &mut bytes);
            let mut back = [9.0; 7];
            assert_eq!(enc.decode(&bytes[..n], &mut back), 7);
            assert_eq!(back, samples, "{enc:?}");
        }
    }

    #[test]
    fn encode_and_decode_stop_at_buffer_end() {
        let mut out = [0u8; 5];
        assert_eq!(Encoding::L16.encode(&[0.1; 4], &mut out), 4);
        let mut back = [0.0; 1];
        assert_eq!(Encoding::L16.decode(&[0; 5], &mut back), 1);
    }

    #[test]
    fn sequence_loss_and_reorder() {
        let mut s = SequenceStats::default();
        for seq in [65534, 65535, 1, 0, 3] {
            s.record(seq);
        }
        // 65534..=3 is 6 packets, 2 missing.
        assert_eq!(s.received(), 5);
        assert_eq!(s.lost(), 1);
        s.record(2);
        assert_eq!(s.lost(), 0);
    }
}
