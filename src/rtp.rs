/// RTP packet parsing for ka9q-radio audio streams.
///
/// All RTP payload types are accepted; filtering by channel count
/// is done at a higher level using per-SSRC metadata from the ka9q-radio status
/// stream.  There is **no** TLV metadata embedded inside data packets — all
/// channel metadata (frequency, sample rate, encoding, PT) arrives on the
/// separate status port.
///
/// The RTP fixed header is always 12 bytes (RFC 3550 §5.1).  CSRC list and
/// header extension are handled but their content is not used.
use crate::error::WsprError;

/// Decoded RTP fixed header fields.
#[derive(Debug, Clone, PartialEq)]
pub struct RtpHeader {
    /// RTP version — must be 2.
    pub version: u8,
    /// Padding bit.
    pub padding: bool,
    /// Extension bit.
    pub extension: bool,
    /// Number of CSRC identifiers following the fixed header.
    pub csrc_count: u8,
    /// Marker bit (payload-type-specific meaning).
    pub marker: bool,
    /// Payload type identifier.
    pub payload_type: u8,
    /// Sequence number (wraps at 65535).
    pub sequence: u16,
    /// Timestamp (sample clock, payload-type-specific units).
    pub timestamp: u32,
    /// Synchronisation source identifier.
    pub ssrc: u32,
}

/// A parsed RTP packet holding a zero-copy reference into the receive buffer.
///
/// `payload` is a slice into the original datagram; no samples are copied
/// until the caller hands them to [`buffer::AudioWindow`](crate::buffer::AudioWindow).
#[derive(Debug)]
pub struct RtpPacket<'buf> {
    /// Decoded fixed header.
    pub header: RtpHeader,
    /// Raw payload bytes (format depends on the encoding reported in the status stream).
    pub payload: &'buf [u8],
}

/// Parse a raw UDP datagram into an [`RtpPacket`].
///
/// Accepts any RTP payload type.  Callers should consult the per-SSRC
/// [`ChannelInfo`](crate::status::ChannelInfo) from the status stream to
/// determine whether the payload is actually IQ data and which byte order
/// to use.
///
/// # Arguments
///
/// * `buf` — the full UDP payload, borrowed for `'buf`
///
/// # Errors
///
/// * [`WsprError::PacketTooShort`] if the buffer is shorter than 12 bytes or
///   the CSRC / extension block overruns it
/// * [`WsprError::UnsupportedRtpVersion`] if the version field != 2
pub fn parse_rtp_packet(buf: &[u8]) -> Result<RtpPacket<'_>, WsprError> {
    // ---- fixed 12-byte header ----
    if buf.len() < 12 {
        return Err(WsprError::PacketTooShort(buf.len()));
    }

    // Byte 0: V(2) P(1) X(1) CC(4)
    let version = (buf[0] >> 6) & 0x03;
    let padding = (buf[0] & 0x20) != 0;
    let extension = (buf[0] & 0x10) != 0;
    let csrc_count = buf[0] & 0x0F;

    if version != 2 {
        return Err(WsprError::UnsupportedRtpVersion(version));
    }

    // Byte 1: M(1) PT(7)
    let marker = (buf[1] & 0x80) != 0;
    let payload_type = buf[1] & 0x7F;

    // Bytes 2-3: sequence; 4-7: timestamp; 8-11: SSRC — all big-endian.
    let sequence = u16::from_be_bytes([buf[2], buf[3]]);
    let timestamp = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let ssrc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);

    let header = RtpHeader {
        version,
        padding,
        extension,
        csrc_count,
        marker,
        payload_type,
        sequence,
        timestamp,
        ssrc,
    };

    // Advance past the fixed header.
    let mut pos = 12usize;

    // Skip CSRC list (each entry is 4 bytes).
    pos += (csrc_count as usize) * 4;

    // Skip optional extension block (RFC 3550 §5.3.1).
    if extension {
        // Need at least 4 bytes for the extension header.
        if pos + 4 > buf.len() {
            return Err(WsprError::PacketTooShort(buf.len()));
        }
        // Extension length is in 32-bit words, encoded in bytes 2-3 of the
        // extension header (big-endian).
        let ext_words = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]) as usize;
        pos += 4 + ext_words * 4;
    }

    if pos > buf.len() {
        return Err(WsprError::PacketTooShort(buf.len()));
    }

    // Everything remaining is the raw payload.
    let payload = &buf[pos..];

    Ok(RtpPacket { header, payload })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid RTP header byte vector.
    fn make_header(pt: u8, seq: u16, ts: u32, ssrc: u32, csrc_count: u8) -> Vec<u8> {
        let mut v = vec![
            // V=2 P=0 X=0 CC
            (2 << 6) | (csrc_count & 0x0F),
            // M=0 PT
            pt & 0x7F,
        ];
        v.extend_from_slice(&seq.to_be_bytes());
        v.extend_from_slice(&ts.to_be_bytes());
        v.extend_from_slice(&ssrc.to_be_bytes());
        v
    }

    #[test]
    fn parse_any_pt_no_payload() {
        // Arrange: minimal packet with PT 123 (stereo IQ in ka9q-radio), no payload bytes.
        let buf = make_header(123, 1, 0, 0xDEAD_BEEF, 0);

        // Act
        let pkt = parse_rtp_packet(&buf).unwrap();

        // Assert
        assert_eq!(pkt.header.payload_type, 123);
        assert_eq!(pkt.header.ssrc, 0xDEAD_BEEF);
        assert_eq!(pkt.payload.len(), 0);
    }

    #[test]
    fn parse_returns_full_payload_for_any_pt() {
        // Arrange: PT 122 (12 kHz mono in ka9q-radio) should parse without error.
        // Filtering out mono streams happens in buffer_task via ChannelInfo, not here.
        let mut buf = make_header(122, 10, 500, 0xABCD_0001, 0);
        buf.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]); // 4 bytes of payload

        // Act
        let pkt = parse_rtp_packet(&buf).unwrap();

        // Assert: full payload returned regardless of PT.
        assert_eq!(pkt.header.payload_type, 122);
        assert_eq!(pkt.header.sequence, 10);
        assert_eq!(pkt.payload, &[0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn too_short_returns_error() {
        // Arrange: 11-byte buffer — one byte short of minimum.
        let buf = [0u8; 11];
        // Act / Assert
        assert!(matches!(
            parse_rtp_packet(&buf),
            Err(WsprError::PacketTooShort(11))
        ));
    }

    #[test]
    fn bad_version_returns_error() {
        // Arrange: version field = 1 (bits 7-6 of byte 0).
        let mut buf = make_header(123, 0, 0, 0, 0);
        buf[0] = (1 << 6) | (buf[0] & 0x3F);
        // Act / Assert
        assert!(matches!(
            parse_rtp_packet(&buf),
            Err(WsprError::UnsupportedRtpVersion(1))
        ));
    }

    #[test]
    fn csrc_list_is_skipped() {
        // Arrange: CC=1 → 4 extra CSRC bytes after the fixed header.
        let mut buf = make_header(123, 5, 0, 0, 1);
        buf.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]); // CSRC entry
        buf.extend_from_slice(&[0xAA, 0xBB]); // actual payload

        // Act
        let pkt = parse_rtp_packet(&buf).unwrap();

        // Assert: CSRC bytes are skipped; only the real payload remains.
        assert_eq!(pkt.payload, &[0xAA, 0xBB]);
    }

    #[test]
    fn extension_block_is_skipped() {
        // Arrange: X=1 → extension header present.
        // Extension header: 2-byte profile + 2-byte word-count (BE), then word_count*4 bytes.
        let mut buf = vec![
            (2 << 6) | 0x10, // V=2 X=1 CC=0
            123u8,           // M=0 PT=123
            0x00,
            0x01, // seq=1
            0x00,
            0x00,
            0x00,
            0x00, // ts=0
            0x00,
            0x00,
            0x00,
            0x01, // ssrc=1
        ];
        // Extension header: profile=0x0000, length=1 (word = 4 bytes)
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        buf.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]); // the one extension word
        buf.extend_from_slice(&[0x11, 0x22]); // actual payload

        // Act
        let pkt = parse_rtp_packet(&buf).unwrap();

        // Assert: extension block skipped; payload is the last two bytes.
        assert_eq!(pkt.payload, &[0x11, 0x22]);
    }
}
