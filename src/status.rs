/// ka9q-radio status stream parser.
///
/// The status stream arrives on a separate UDP port (default 5006) from the same
/// multicast group as the RTP data stream (default 5004).  Each datagram is a
/// TLV-encoded packet describing the current state of one or more output channels.
///
/// # Packet structure
///
/// ```text
/// byte 0:    pkt_type (0 = STATUS, 1 = CMD — only STATUS is processed here)
/// bytes 1+:  TLV chain
///            [tag: u8] [length: variable] [value: big-endian, leading-zero bytes suppressed]
///            tag 0 is the EOL sentinel (no length/value follows)
/// ```
///
/// # Length encoding
///
/// - If the first byte < 0x80 → literal byte count.
/// - If the first byte >= 0x80 → `first_byte = 0x80 | N` where N (1–4) is the
///   number of following big-endian bytes that hold the actual length.
///
/// # Value encoding
///
/// All multi-byte integers and floats are big-endian with leading zero bytes
/// suppressed.  A zero value is represented by a zero-length field.
/// Doubles are stored as their `u64` bit-pattern encoded the same way; decode
/// with `f64::from_bits`.
use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Notify, RwLock};

// ---- public encoding constants (used by buffer.rs for byte-order dispatch) ----

/// Encoding identifier for 16-bit signed little-endian PCM.
/// Supported as a fallback in [`buffer::AudioWindow::ingest`]; not currently
/// emitted by ka9q-radio but retained for compatibility.
#[allow(dead_code)]
pub const ENC_S16LE: u8 = 1;
/// Encoding identifier for 16-bit signed big-endian PCM (used by ka9q-radio audio output).
pub const ENC_S16BE: u8 = 2;

// ---- private status-packet type ----
const PKT_STATUS: u8 = 0;

// ---- private TLV tag constants (from ka9q-radio include/status.h) ----
const TAG_OUTPUT_SSRC: u8 = 18;
const TAG_OUTPUT_SAMPRATE: u8 = 20;
const TAG_RADIO_FREQUENCY: u8 = 33;
const TAG_OUTPUT_CHANNELS: u8 = 49;
const TAG_RTP_PT: u8 = 105;
const TAG_OUTPUT_ENCODING: u8 = 107;

/// Per-channel metadata extracted from the ka9q-radio status stream.
///
/// One `ChannelInfo` is maintained per SSRC; fields are updated as status
/// packets arrive.  The channel is ready for audio ingestion once
/// [`is_audio_ready`](ChannelInfo::is_audio_ready) returns `true`.
#[derive(Debug, Clone, Default)]
pub struct ChannelInfo {
    /// RTP payload type in use on the companion data stream.
    pub pt: u8,
    /// Centre frequency in Hz (from `RADIO_FREQUENCY` TLV tag).
    pub center_freq_hz: f64,
    /// Output sample rate in Hz (from `OUTPUT_SAMPRATE` TLV tag).
    pub sample_rate_hz: u32,
    /// Number of channels: 1 = mono (demodulated audio), 2 = stereo IQ.
    pub channels: u8,
    /// Sample encoding: see `ENC_S16BE`, `ENC_S16LE`, etc.
    pub encoding: u8,
}

impl ChannelInfo {
    /// Returns `true` when enough metadata is known to begin audio ingestion.
    ///
    /// Requires:
    /// - `channels == 1` (mono USB audio — the standard ka9q-radio WSPR output)
    /// - `sample_rate_hz > 0`
    /// - `center_freq_hz > 0.0`
    pub fn is_audio_ready(&self) -> bool {
        self.channels == 1 && self.sample_rate_hz > 0 && self.center_freq_hz > 0.0
    }
}

/// Intermediate accumulator for a single status packet's worth of updates.
///
/// All fields are optional; only tags that were actually present in the packet
/// end up as `Some(_)`.
struct StatusUpdate {
    ssrc: Option<u32>,
    center_freq_hz: Option<f64>,
    sample_rate_hz: Option<u32>,
    channels: Option<u8>,
    pt: Option<u8>,
    encoding: Option<u8>,
}

/// Parse a status UDP datagram and apply any updates to `map`.
///
/// # Arguments
///
/// * `buf` — raw datagram bytes
/// * `map` — per-SSRC channel info map; updated in place
///
/// # Returns
///
/// The SSRC found in the packet, or `None` if the packet was not a valid
/// STATUS datagram or contained no `OUTPUT_SSRC` tag.
pub fn process_status_packet(buf: &[u8], map: &mut HashMap<u32, ChannelInfo>) -> Option<u32> {
    if buf.is_empty() || buf[0] != PKT_STATUS {
        return None;
    }

    let mut pos = 1usize; // advance past pkt_type byte
    let mut update = StatusUpdate {
        ssrc: None,
        center_freq_hz: None,
        sample_rate_hz: None,
        channels: None,
        pt: None,
        encoding: None,
    };

    // Walk the TLV chain until EOL or end of buffer.
    loop {
        if pos >= buf.len() {
            break;
        }
        let tag = buf[pos];
        pos += 1;

        // Tag 0 is the EOL sentinel — no length or value follows.
        if tag == 0 {
            break;
        }

        // Decode the variable-length length field.
        let len = match read_tlv_len(buf, &mut pos) {
            Some(l) => l,
            None => break, // malformed — stop processing
        };

        // Bounds-check the value slice.
        if pos + len > buf.len() {
            break; // malformed — stop processing
        }
        let value = &buf[pos..pos + len];
        pos += len;

        match tag {
            TAG_OUTPUT_SSRC => {
                update.ssrc = Some(decode_u32(value));
            }
            TAG_OUTPUT_SAMPRATE => {
                update.sample_rate_hz = Some(decode_u32(value));
            }
            TAG_RADIO_FREQUENCY => {
                update.center_freq_hz = Some(decode_f64(value));
            }
            TAG_OUTPUT_CHANNELS => {
                update.channels = Some(decode_u32(value) as u8);
            }
            TAG_RTP_PT => {
                // PT is a single byte; treat as u8.
                update.pt = Some(decode_u32(value) as u8);
            }
            TAG_OUTPUT_ENCODING => {
                update.encoding = Some(decode_u32(value) as u8);
            }
            // Unknown tags are safely skipped via the length field.
            _ => {}
        }
    }

    let ssrc = update.ssrc?;

    // Apply non-None fields to the map entry for this SSRC.
    // `entry().or_default()` inserts a zeroed ChannelInfo if the SSRC is new.
    let info = map.entry(ssrc).or_default();
    if let Some(v) = update.center_freq_hz {
        info.center_freq_hz = v;
    }
    if let Some(v) = update.sample_rate_hz {
        info.sample_rate_hz = v;
    }
    if let Some(v) = update.channels {
        info.channels = v;
    }
    if let Some(v) = update.pt {
        info.pt = v;
    }
    if let Some(v) = update.encoding {
        info.encoding = v;
    }

    Some(ssrc)
}

/// Async receive loop for the ka9q-radio status multicast stream.
///
/// Reads UDP datagrams from `socket`, calls [`process_status_packet`] for each
/// one, and updates `channel_map` under a write lock.  Runs until `shutdown` is
/// notified.
///
/// # Arguments
///
/// * `socket`      — bound UDP socket on the status port
/// * `channel_map` — shared map of per-SSRC channel metadata
/// * `shutdown`    — shared `Notify`; notified to request clean shutdown
pub async fn receive_loop(
    socket: tokio::net::UdpSocket,
    channel_map: Arc<RwLock<HashMap<u32, ChannelInfo>>>,
    shutdown: Arc<Notify>,
) {
    // Reuse a single heap allocation for the receive buffer.
    let mut buf = vec![0u8; 65_535];

    loop {
        tokio::select! {
            biased;

            _ = shutdown.notified() => {
                tracing::info!("status receive_loop: shutdown signal received");
                break;
            }

            result = socket.recv_from(&mut buf) => {
                match result {
                    Ok((len, src)) => {
                        tracing::debug!(
                            src = %src,
                            len,
                            first_byte = buf.first().copied().unwrap_or(0xFF),
                            "status packet received"
                        );
                        let mut map = channel_map.write().await;

                        // Snapshot per-SSRC readiness BEFORE applying the update
                        // so we can detect the not-ready → ready transition and
                        // log it exactly once at info level.  Periodic heartbeats
                        // for already-ready channels are demoted to debug.
                        let pre_ready = peek_ssrc(&buf[..len])
                            .map(|s| map.get(&s).is_some_and(|i| i.is_audio_ready()));

                        match process_status_packet(&buf[..len], &mut map) {
                            None => {
                                tracing::debug!(
                                    src = %src,
                                    len,
                                    first_byte = buf.first().copied().unwrap_or(0xFF),
                                    "status packet ignored (not STATUS type or no SSRC tag)"
                                );
                            }
                            Some(ssrc) => {
                                if let Some(info) = map.get(&ssrc) {
                                    if info.is_audio_ready()
                                        && !pre_ready.unwrap_or(false)
                                    {
                                        // First time this channel became ready.
                                        tracing::info!(
                                            ssrc,
                                            freq_hz     = info.center_freq_hz,
                                            sample_rate = info.sample_rate_hz,
                                            channels    = info.channels,
                                            pt          = info.pt,
                                            encoding    = info.encoding,
                                            "audio channel ready"
                                        );
                                    } else {
                                        tracing::debug!(
                                            ssrc,
                                            freq_hz     = info.center_freq_hz,
                                            sample_rate = info.sample_rate_hz,
                                            channels    = info.channels,
                                            "status update"
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "status recv_from error");
                    }
                }
            }
        }
    }
}

// ---- private helpers ----

/// Extract only the `OUTPUT_SSRC` tag value from a STATUS packet without
/// modifying any map.  Returns `None` if the packet is not a STATUS packet
/// or contains no `OUTPUT_SSRC` tag.
///
/// Used to snapshot per-SSRC readiness before calling
/// [`process_status_packet`] so we can detect the first-ready transition.
fn peek_ssrc(buf: &[u8]) -> Option<u32> {
    if buf.is_empty() || buf[0] != PKT_STATUS {
        return None;
    }
    let mut pos = 1usize;
    loop {
        if pos >= buf.len() {
            break;
        }
        let tag = buf[pos];
        pos += 1;
        if tag == 0 {
            break;
        }
        let len = read_tlv_len(buf, &mut pos)?;
        if pos + len > buf.len() {
            break;
        }
        if tag == TAG_OUTPUT_SSRC {
            return Some(decode_u32(&buf[pos..pos + len]));
        }
        pos += len;
    }
    None
}

/// Decode the TLV variable-length length field at `buf[*pos]`.
///
/// Advances `*pos` past the length bytes consumed.
///
/// - If `buf[*pos] < 0x80`: length = that byte (1 byte consumed).
/// - Else: low 7 bits = N (number of following BE bytes); read N bytes for length.
///
/// Returns `None` if the encoding is invalid or the buffer is too short.
fn read_tlv_len(buf: &[u8], pos: &mut usize) -> Option<usize> {
    if *pos >= buf.len() {
        return None;
    }
    let first = buf[*pos];
    *pos += 1;

    if first < 0x80 {
        // Short form: literal length.
        return Some(first as usize);
    }

    // Long form: the low 7 bits give the number of following bytes.
    let n = (first & 0x7F) as usize;
    if n == 0 || n > 4 || *pos + n > buf.len() {
        return None;
    }

    let mut len = 0usize;
    for &byte in &buf[*pos..*pos + n] {
        len = (len << 8) | (byte as usize);
    }
    *pos += n;
    Some(len)
}

/// Decode a big-endian unsigned integer from a leading-zero-suppressed byte slice.
///
/// An empty slice decodes to 0.  At most 4 bytes are meaningful (u32 range).
fn decode_u32(value: &[u8]) -> u32 {
    let mut acc = 0u64;
    for &byte in value {
        acc = (acc << 8) | (byte as u64);
    }
    acc as u32
}

/// Decode an `f64` from a leading-zero-suppressed big-endian u64 bit-pattern.
///
/// An empty slice decodes to `0.0`.  The bytes are accumulated as a big-endian
/// `u64` and then reinterpreted via `f64::from_bits`.
fn decode_f64(value: &[u8]) -> f64 {
    if value.is_empty() {
        return 0.0;
    }
    let mut bits = 0u64;
    for &byte in value {
        bits = (bits << 8) | (byte as u64);
    }
    f64::from_bits(bits)
}

// ---- unit tests ----

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a STATUS packet (pkt_type=0) with the given tag/value pairs.
    ///
    /// Uses 1-byte length encoding (values must be < 128 bytes).
    fn make_status_packet(fields: &[(u8, &[u8])]) -> Vec<u8> {
        let mut pkt = vec![PKT_STATUS];
        for &(tag, value) in fields {
            pkt.push(tag);
            // 1-byte length (short form, value < 128 bytes).
            assert!(
                value.len() < 128,
                "test helper: value too long for short-form length"
            );
            pkt.push(value.len() as u8);
            pkt.extend_from_slice(value);
        }
        // EOL sentinel.
        pkt.push(0u8);
        pkt
    }

    // ---- decode_u32 ----

    #[test]
    fn decode_u32_empty_is_zero() {
        // Arrange / Act / Assert
        assert_eq!(decode_u32(&[]), 0);
    }

    #[test]
    fn decode_u32_single_byte() {
        // Arrange: single byte [5] represents the value 5 (leading zeroes suppressed).
        // Act / Assert
        assert_eq!(decode_u32(&[5u8]), 5);
    }

    #[test]
    fn decode_u32_multi_byte() {
        // Arrange: 12000 = 0x00002EE0 — suppressed leading zero → [0x2E, 0xE0].
        let value = 12_000u32.to_be_bytes();
        // Strip leading zeroes as ka9q-radio would.
        let stripped: Vec<u8> = {
            let s = value
                .iter()
                .skip_while(|&&b| b == 0)
                .copied()
                .collect::<Vec<_>>();
            if s.is_empty() {
                vec![0]
            } else {
                s
            }
        };
        // Act
        let result = decode_u32(&stripped);
        // Assert
        assert_eq!(result, 12_000);
    }

    // ---- decode_f64 ----

    #[test]
    fn decode_f64_empty_is_zero() {
        assert_eq!(decode_f64(&[]), 0.0);
    }

    #[test]
    fn decode_f64_roundtrips_14097000() {
        // Arrange: encode 14_097_000.0 Hz as BE u64 bits with leading zero suppression.
        let bits = 14_097_000.0f64.to_bits();
        let be_bytes = bits.to_be_bytes();
        let stripped: Vec<u8> = be_bytes.iter().skip_while(|&&b| b == 0).copied().collect();

        // Act
        let result = decode_f64(&stripped);

        // Assert
        assert_eq!(result, 14_097_000.0f64);
    }

    // ---- read_tlv_len ----

    #[test]
    fn read_tlv_len_short_form() {
        // Arrange: first byte < 0x80 → literal length.
        let buf = [42u8, 0xFF];
        let mut pos = 0;
        // Act
        let len = read_tlv_len(&buf, &mut pos);
        // Assert
        assert_eq!(len, Some(42));
        assert_eq!(pos, 1);
    }

    #[test]
    fn read_tlv_len_long_form_two_bytes() {
        // Arrange: 0x82 means "2 following bytes for length", value = [0x01, 0x00] = 256.
        let buf = [0x82u8, 0x01, 0x00];
        let mut pos = 0;
        // Act
        let len = read_tlv_len(&buf, &mut pos);
        // Assert
        assert_eq!(len, Some(256));
        assert_eq!(pos, 3);
    }

    #[test]
    fn read_tlv_len_long_form_invalid_n_zero() {
        // Arrange: 0x80 means "0 following bytes" — invalid.
        let buf = [0x80u8];
        let mut pos = 0;
        assert_eq!(read_tlv_len(&buf, &mut pos), None);
    }

    // ---- process_status_packet ----

    #[test]
    fn ignores_cmd_packets() {
        // Arrange: pkt_type = 1 (CMD), not STATUS.
        let pkt = vec![1u8, 0u8]; // CMD + EOL
        let mut map = HashMap::new();
        // Act
        let result = process_status_packet(&pkt, &mut map);
        // Assert: must return None and leave map untouched.
        assert!(result.is_none());
        assert!(map.is_empty());
    }

    #[test]
    fn eol_only_packet_returns_none() {
        // Arrange: STATUS packet with only the EOL sentinel (no SSRC tag).
        let pkt = vec![PKT_STATUS, 0u8];
        let mut map = HashMap::new();
        // Act
        let result = process_status_packet(&pkt, &mut map);
        // Assert: no SSRC → None.
        assert!(result.is_none());
    }

    #[test]
    fn partial_packet_ssrc_only() {
        // Arrange: STATUS packet with only SSRC tag — freq/rate/channels absent.
        let ssrc_val = 0xDEAD_BEEFu32.to_be_bytes();
        let pkt = make_status_packet(&[(TAG_OUTPUT_SSRC, &ssrc_val)]);
        let mut map = HashMap::new();
        // Act
        let result = process_status_packet(&pkt, &mut map);
        // Assert: SSRC returned; entry created but not iq_ready.
        assert_eq!(result, Some(0xDEAD_BEEF));
        let info = map.get(&0xDEAD_BEEF).expect("entry should exist");
        assert!(!info.is_audio_ready());
    }

    #[test]
    fn parses_full_channel_info() {
        // Arrange: build a packet with SSRC, freq, sample_rate, channels=1 (mono audio), PT, encoding.
        let ssrc: u32 = 0x0000_1234;
        let freq_bits = 14_095_600.0f64.to_bits(); // USB dial frequency

        // Strip leading zeros from each value as ka9q-radio would.
        let ssrc_bytes = ssrc.to_be_bytes();
        let ssrc_stripped: Vec<u8> = ssrc_bytes
            .iter()
            .skip_while(|&&b| b == 0)
            .copied()
            .collect();

        let freq_be = freq_bits.to_be_bytes();
        let freq_stripped: Vec<u8> = freq_be.iter().skip_while(|&&b| b == 0).copied().collect();

        let rate_bytes = 12_000u32.to_be_bytes();
        let rate_stripped: Vec<u8> = rate_bytes
            .iter()
            .skip_while(|&&b| b == 0)
            .copied()
            .collect();

        let pkt = make_status_packet(&[
            (TAG_OUTPUT_SSRC, &ssrc_stripped),
            (TAG_RADIO_FREQUENCY, &freq_stripped),
            (TAG_OUTPUT_SAMPRATE, &rate_stripped),
            (TAG_OUTPUT_CHANNELS, &[1u8]),   // mono audio (USB demod)
            (TAG_RTP_PT, &[122u8]),          // PT 122 = 12 kHz mono
            (TAG_OUTPUT_ENCODING, &[ENC_S16BE]),
        ]);

        let mut map = HashMap::new();
        // Act
        let result = process_status_packet(&pkt, &mut map);

        // Assert
        assert_eq!(result, Some(0x0000_1234));
        let info = map.get(&0x0000_1234).expect("entry should exist");
        assert_eq!(info.center_freq_hz, 14_095_600.0);
        assert_eq!(info.sample_rate_hz, 12_000);
        assert_eq!(info.channels, 1);
        assert_eq!(info.pt, 122);
        assert_eq!(info.encoding, ENC_S16BE);
        assert!(info.is_audio_ready());
    }

    #[test]
    fn unknown_tags_are_skipped() {
        // Arrange: a packet with an unknown tag (200) followed by valid SSRC.
        let ssrc_bytes = 42u32.to_be_bytes();
        let pkt = make_status_packet(&[
            (200u8, &[0xDE, 0xAD]), // unknown tag — must be safely skipped
            (TAG_OUTPUT_SSRC, &ssrc_bytes),
        ]);
        let mut map = HashMap::new();
        // Act
        let result = process_status_packet(&pkt, &mut map);
        // Assert: SSRC was still found after skipping the unknown tag.
        assert_eq!(result, Some(42));
    }

    #[test]
    fn is_audio_ready_requires_mono() {
        // Arrange: stereo channel (channels == 2) must NOT be audio_ready.
        let mut info = ChannelInfo::default();
        info.channels = 2;
        info.sample_rate_hz = 12_000;
        info.center_freq_hz = 14_095_600.0;
        // Act / Assert
        assert!(!info.is_audio_ready());
    }

    #[test]
    fn is_audio_ready_requires_nonzero_freq_and_rate() {
        // Arrange: mono but missing freq and rate.
        let mut info = ChannelInfo::default();
        info.channels = 1;
        // center_freq_hz = 0.0, sample_rate_hz = 0 (defaults)
        assert!(!info.is_audio_ready());

        // Set only sample_rate.
        info.sample_rate_hz = 12_000;
        assert!(!info.is_audio_ready());

        // Set both.
        info.center_freq_hz = 14_095_600.0;
        assert!(info.is_audio_ready());
    }
}
