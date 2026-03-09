/// Mono audio buffering with 2-minute WSPR window alignment.
///
/// WSPR transmissions always begin on even UTC minutes.  [`AudioWindow`] holds
/// pre-allocated mono PCM samples (`i16`) for exactly one such window.
/// Gaps (missing RTP sequence numbers) are left as pre-zeroed silence, which
/// `wsprd` handles correctly.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::WsprError;

/// Number of RTP sample ticks in 2^32 — used for wrap-around arithmetic on
/// 32-bit RTP timestamps.  ka9q-radio's RTP clock origin is arbitrary, so we
/// never compare timestamps to wall-clock absolute values; instead we track the
/// first timestamp seen and compute all subsequent offsets relative to it.
const RTP_WRAP: i64 = 1i64 << 32;

/// One 2-minute mono audio capture window for a single SSRC.
///
/// Samples are stored as `i16` PCM values at `sample_rate_hz` per second.
/// The buffer is pre-allocated at construction time to avoid reallocation
/// during ingestion.
#[derive(Debug)]
pub struct AudioWindow {
    /// UTC timestamp of the start of this window (aligned to an even minute).
    pub window_start: SystemTime,

    /// Dial/carrier frequency in Hz, from the ka9q-radio status stream.
    /// For USB-mode channels this is the USB carrier frequency; `wsprd`
    /// uses it as the dial frequency when computing spot frequencies.
    pub center_freq_hz: f64,

    /// Sample rate in Hz, from the ka9q-radio status stream.
    pub sample_rate_hz: u32,

    /// Pre-allocated PCM storage: `capacity_samples` i16 values.
    samples: Vec<i16>,

    /// Number of samples written so far (high-water mark).
    pub samples_written: usize,

    /// Total sample capacity: `sample_rate_hz * capture_seconds`.
    pub capacity_samples: usize,

    /// Last RTP sequence number seen, used for gap detection.
    pub last_seq: Option<u16>,

    /// Cumulative number of detected sequence gaps.
    pub gap_count: u64,

    /// RTP timestamp of the very first packet ingested into this window.
    /// All subsequent offsets are computed *relative* to this value, which
    /// eliminates any dependency on ka9q-radio's clock epoch.
    rtp_base: Option<u32>,

    /// Sample-buffer index at which the first packet was placed, derived from
    /// wall-clock elapsed time within the window at the moment the first packet
    /// arrived.  Subsequent packets are placed at `start_offset + Δrtp_samples`.
    start_offset: usize,
}

impl AudioWindow {
    /// Allocate a new window aligned to the given even UTC minute boundary.
    ///
    /// # Arguments
    ///
    /// * `window_start`    — the even-minute UTC timestamp for this window
    /// * `center_freq_hz`  — dial/carrier frequency in Hz (from the status stream)
    /// * `sample_rate_hz`  — PCM sample rate in samples per second
    /// * `capture_seconds` — number of seconds to capture
    pub fn new(
        window_start: SystemTime,
        center_freq_hz: f64,
        sample_rate_hz: u32,
        capture_seconds: u32,
    ) -> Self {
        let capacity = sample_rate_hz as usize * capture_seconds as usize;
        Self {
            window_start,
            center_freq_hz,
            sample_rate_hz,
            // One i16 per mono sample.
            samples: vec![0i16; capacity],
            samples_written: 0,
            capacity_samples: capacity,
            last_seq: None,
            gap_count: 0,
            rtp_base: None,
            start_offset: 0,
        }
    }

    /// Ingest raw mono PCM bytes from an RTP packet.
    ///
    /// Samples are placed at an offset computed as follows:
    ///
    /// * **First packet** — the write position is derived from the wall-clock
    ///   elapsed time since `window_start`, so a late-joining stream lands at
    ///   the correct position regardless of ka9q-radio's RTP clock epoch.
    /// * **Subsequent packets** — the offset is `start_offset + (rtp_timestamp
    ///   − rtp_base)` where `rtp_base` is the first packet's timestamp.  This
    ///   is purely relative arithmetic and does not assume any particular epoch.
    ///
    /// Gaps (missing sequence numbers) are left as the pre-zeroed silence that
    /// `wsprd` handles correctly.
    ///
    /// The byte order is selected via the `encoding` parameter:
    /// use [`ENC_S16BE`](crate::status::ENC_S16BE) for ka9q-radio audio output
    /// (big-endian) or [`ENC_S16LE`](crate::status::ENC_S16LE) as a fallback.
    ///
    /// # Arguments
    ///
    /// * `payload`        — raw i16 bytes from the RTP payload (2 bytes per sample)
    /// * `rtp_timestamp`  — RTP timestamp field from the packet header
    /// * `sample_rate_hz` — sample rate used to convert RTP deltas to sample counts
    /// * `rtp_seq`        — RTP sequence number for gap tracking
    /// * `ssrc`           — SSRC (used only for error context)
    /// * `encoding`       — sample encoding (see `crate::status::ENC_*` constants)
    ///
    /// # Errors
    ///
    /// Returns [`WsprError::BufferOverflow`] if the computed write offset
    /// plus the payload length would exceed the pre-allocated capacity.
    pub fn ingest(
        &mut self,
        payload: &[u8],
        rtp_timestamp: u32,
        sample_rate_hz: u32,
        rtp_seq: u16,
        ssrc: u32,
        encoding: u8,
    ) -> Result<(), WsprError> {
        // ---- gap detection ----
        if let Some(last) = self.last_seq {
            // Sequence numbers wrap at u16::MAX.  A difference > 1 means we
            // dropped packets.
            let expected = last.wrapping_add(1);
            if rtp_seq != expected {
                self.gap_count += 1;
                tracing::warn!(
                    ssrc,
                    expected_seq = expected,
                    got_seq = rtp_seq,
                    "RTP sequence gap detected"
                );
            }
        }
        self.last_seq = Some(rtp_seq);

        // ---- timestamp → buffer offset ----
        //
        // ka9q-radio's RTP clock epoch is arbitrary (it does NOT start from
        // Unix epoch × sample_rate).  We therefore never compare RTP timestamps
        // to wall-clock absolute values.  Instead:
        //
        //   1. On the *first* packet in this window we derive the write position
        //      purely from wall-clock elapsed time since the window boundary.
        //      That gives us `start_offset` in samples.
        //
        //   2. Every *subsequent* packet is placed at
        //        offset = start_offset + (rtp_timestamp - rtp_base) [mod 2^32]
        //      This is purely relative RTP arithmetic and is immune to clock-
        //      epoch mismatches.
        //
        // All arithmetic is i64 to handle u32 wrap-around (can happen for
        // streams running > ~35 minutes at 192 kHz, well within a session).

        let offset: usize = match self.rtp_base {
            None => {
                // First packet: anchor the RTP clock to wall time.
                let elapsed_secs = SystemTime::now()
                    .duration_since(self.window_start)
                    .unwrap_or(Duration::ZERO)
                    .as_secs_f64();
                // Clamp to [0, capacity) so a slightly-early packet doesn't
                // produce a negative offset.
                let wall_offset =
                    ((elapsed_secs * sample_rate_hz as f64) as i64).max(0) as usize;
                let wall_offset = wall_offset.min(self.capacity_samples.saturating_sub(1));

                self.rtp_base = Some(rtp_timestamp);
                self.start_offset = wall_offset;
                wall_offset
            }
            Some(base) => {
                // Subsequent packets: relative RTP displacement from the base.
                let mut delta = rtp_timestamp as i64 - base as i64;
                // Correct for 32-bit wrap-around.  A genuine backwards jump of
                // more than half the 32-bit range is implausible within a
                // 2-minute window, so we treat it as a forward wrap.
                if delta < -(RTP_WRAP / 2) {
                    delta += RTP_WRAP;
                }
                if delta < 0 {
                    // Packet arrived slightly before the anchor (reorder or
                    // duplicate); discard.
                    return Ok(());
                }
                let abs = self.start_offset as i64 + delta;
                if abs < 0 {
                    return Ok(());
                }
                abs as usize
            }
        };

        // Number of mono samples in this packet: 2 bytes each.
        let n_samples = payload.len() / 2;

        if offset + n_samples > self.capacity_samples {
            return Err(WsprError::BufferOverflow { ssrc });
        }

        // Select the byte-order decoder once before the loop.
        // ka9q-radio audio uses S16BE; S16LE is supported as fallback.
        let decode_sample: fn([u8; 2]) -> i16 = if encoding == crate::status::ENC_S16BE {
            i16::from_be_bytes
        } else {
            i16::from_le_bytes
        };

        for (i, chunk) in payload.chunks_exact(2).enumerate() {
            self.samples[offset + i] = decode_sample([chunk[0], chunk[1]]);
        }

        // Track the high-water mark (not necessarily monotone if out-of-order
        // packets arrive, but good enough for progress display).
        self.samples_written = self.samples_written.max(offset + n_samples);

        Ok(())
    }

    /// Return the buffered PCM samples as a slice.
    ///
    /// The slice length is always `capacity_samples`; positions not yet written
    /// contain zeroed silence.
    pub fn samples(&self) -> &[i16] {
        &self.samples
    }

    /// Fraction of the window that has been filled (0.0 to 1.0).
    pub fn fill_fraction(&self) -> f32 {
        self.samples_written as f32 / self.capacity_samples as f32
    }
}

/// Compute the UTC `SystemTime` for the most recent even-minute boundary
/// at or before `now`.
///
/// WSPR transmissions start on even UTC minutes, so we align windows to the
/// most recent even minute.
///
/// # Arguments
///
/// * `now` — the current time (typically `SystemTime::now()`)
///
/// # Returns
///
/// `SystemTime` floored to the nearest even minute boundary <= `now`.
pub fn even_minute_boundary(now: SystemTime) -> SystemTime {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    // Round down to the nearest even minute.
    let aligned = (secs / 120) * 120;
    UNIX_EPOCH + Duration::from_secs(aligned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::ENC_S16BE;

    #[test]
    fn even_minute_boundary_aligns_correctly() {
        // Arrange: 2024-01-01 00:01:30 UTC = 1704067290 seconds.
        let t = UNIX_EPOCH + Duration::from_secs(1_704_067_290);
        // Act
        let boundary = even_minute_boundary(t);
        let secs = boundary.duration_since(UNIX_EPOCH).unwrap().as_secs();
        // Assert: floor to 00:00:00 UTC (1704067200).
        assert_eq!(
            secs % 120,
            0,
            "boundary must be divisible by 120 (2 minutes)"
        );
        assert!(secs <= 1_704_067_290, "boundary must not exceed now");
    }

    #[test]
    fn even_minute_boundary_on_boundary_is_itself() {
        // Arrange: exactly on a 2-minute boundary.
        let secs = 1_704_067_200u64; // already divisible by 120
        let t = UNIX_EPOCH + Duration::from_secs(secs);
        // Act
        let boundary = even_minute_boundary(t);
        // Assert
        assert_eq!(boundary.duration_since(UNIX_EPOCH).unwrap().as_secs(), secs);
    }

    #[test]
    fn window_capacity_is_correct() {
        // Arrange / Act
        let start = UNIX_EPOCH;
        let w = AudioWindow::new(start, 14_095_600.0, 12_000, 116);
        // Assert: mono — one i16 per sample, no stereo multiplier.
        assert_eq!(w.capacity_samples, 12_000 * 116);
        assert_eq!(w.samples.len(), 12_000 * 116);
        assert_eq!(w.fill_fraction(), 0.0);
    }

    #[test]
    fn ingest_fills_samples_at_correct_offset() {
        // Arrange: window_start = now so wall-clock elapsed ≈ 0 → start_offset = 0.
        // The first RTP timestamp becomes the base; the sample lands at index 0.
        let start = SystemTime::now();
        let mut w = AudioWindow::new(start, 14_095_600.0, 12_000, 116);

        // One mono sample: value 1000, encoded as S16BE.
        let payload = 1000i16.to_be_bytes().to_vec();

        // Act
        w.ingest(&payload, 0, 12_000, 0, 0xABCD, ENC_S16BE).unwrap();

        // Assert: elapsed is sub-millisecond so start_offset rounds to 0.
        let samples = w.samples();
        assert_eq!(samples[0], 1000i16, "first sample must be 1000");
        assert_eq!(w.samples_written, 1);
    }

    #[test]
    fn ingest_detects_gaps() {
        // Arrange: window_start = now so start_offset ≈ 0.
        let start = SystemTime::now();
        let mut w = AudioWindow::new(start, 14_095_600.0, 12_000, 116);
        // 2-byte mono payload (one sample)
        let dummy_payload = vec![0u8; 2];

        // Act: send seq 0 (rtp_ts = 0) then jump to seq 5 (gap of 4).
        // Second packet's rtp delta = 2 samples forward.
        w.ingest(&dummy_payload, 0, 12_000, 0, 0, ENC_S16BE).unwrap();
        w.ingest(&dummy_payload, 2, 12_000, 5, 0, ENC_S16BE).unwrap();

        // Assert
        assert_eq!(w.gap_count, 1);
    }

    #[test]
    fn ingest_overflow_returns_error() {
        // Arrange: window_start = now → start_offset ≈ 0.
        // Establish the RTP base with a first packet at timestamp 0, then send
        // a second packet whose RTP delta places it 200 s beyond the buffer.
        let start = SystemTime::now();
        let mut w = AudioWindow::new(start, 14_095_600.0, 12_000, 116);
        let payload = vec![0u8; 2];

        // First packet: anchors rtp_base = 0, start_offset ≈ 0.  Must succeed.
        w.ingest(&payload, 0, 12_000, 0, 0xDEAD, ENC_S16BE).unwrap();

        // Second packet: RTP delta = 200 s × 12000 = 2_400_000 samples, which
        // exceeds capacity_samples (12_000 × 116 = 1_392_000).
        let overflow_ts = 12_000u32 * 200;
        let result = w.ingest(&payload, overflow_ts, 12_000, 1, 0xDEAD, ENC_S16BE);

        // Assert
        assert!(matches!(
            result,
            Err(WsprError::BufferOverflow { ssrc: 0xDEAD })
        ));
    }
}
