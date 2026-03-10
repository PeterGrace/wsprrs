use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A single decoded WSPR spot, populated from `ALL_WSPR.TXT`.
///
/// Fields are sourced from the richer `ALL_WSPR.TXT` format written by
/// wsprd 2.5+, which includes decode-quality metrics not available on stdout.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WsprSpot {
    /// UTC time of the WSPR window, formatted as `HHMM`.
    pub time_utc: String,

    /// Signal-to-noise ratio in dB (relative to 2.5 kHz noise bandwidth).
    pub snr_db: i32,

    /// Time offset from the nominal transmission start in seconds.
    pub dt_sec: f32,

    /// Decoded carrier frequency in Hz.
    pub freq_hz: f64,

    /// Full decoded message string (e.g. `"K1ABC FN42 33"`).
    pub message: String,

    /// Callsign (e.g. `"K1ABC"`).
    pub callsign: String,

    /// Maidenhead grid locator (4 or 6 characters; empty for type-2 messages
    /// that carry only callsign and power with no grid square).
    pub grid: String,

    /// Transmitted power in dBm.
    pub power_dbm: i32,

    /// Frequency drift in Hz/minute.
    pub drift: i32,

    /// Sync quality metric — how cleanly the sync vector locked.
    /// Lower values indicate weaker or noisier signals.
    pub sync_quality: f32,

    /// Number of decode passes required (1 = direct; 3 = required OSD).
    pub npass: u8,

    /// OSD (ordered statistics decode) pass on which the decode succeeded.
    pub osd_pass: u8,

    /// Minimum hard-decision count across the decode.
    /// More negative values indicate a more marginal decode.
    pub nhardmin: i32,

    /// Number of decoder iterations (ncycles in wsprd output).
    /// Higher values mean the decoder worked harder to find the message.
    pub decode_cycles: u32,

    /// Number of candidate messages explored during decode.
    /// High values (thousands) indicate a weak or interference-limited signal.
    pub candidates: u32,

    /// Fano metric for the decoded path.
    /// Large magnitudes indicate a strong, clean decode.
    pub nfano: i32,
}

/// ClickHouse row for a [`WsprSpot`].
///
/// Mirrors all [`WsprSpot`] fields and adds [`window_start_unix`] — a Unix
/// epoch timestamp (seconds) of the WSPR window boundary — which serves as
/// the primary time-series ordering key in ClickHouse.
///
/// This type is intentionally separate from [`WsprSpot`] so that the extra
/// field does not pollute the NDJSON output.
///
/// # ClickHouse DDL
///
/// ```sql
/// CREATE TABLE wspr_spots (
///     window_start_unix Int64,
///     time_utc          String,
///     snr_db            Int32,
///     dt_sec            Float32,
///     freq_hz           Float64,
///     message           String,
///     callsign          String,
///     grid              String,
///     power_dbm         Int32,
///     drift             Int32,
///     sync_quality      Float32,
///     npass             UInt8,
///     osd_pass          UInt8,
///     nhardmin          Int32,
///     decode_cycles     UInt32,
///     candidates        UInt32,
///     nfano             Int32
/// ) ENGINE = MergeTree()
/// ORDER BY (window_start_unix, callsign);
/// ```
#[derive(Debug, Clone, clickhouse::Row, serde::Serialize, serde::Deserialize)]
pub struct WsprSpotRow {
    /// Unix epoch seconds of the WSPR window start.
    pub window_start_unix: i64,
    /// UTC time of the WSPR window, formatted as `HHMM`.
    pub time_utc: String,
    /// Signal-to-noise ratio in dB.
    pub snr_db: i32,
    /// Time offset from the nominal transmission start in seconds.
    pub dt_sec: f32,
    /// Decoded carrier frequency in Hz.
    pub freq_hz: f64,
    /// Full decoded message string.
    pub message: String,
    /// Callsign.
    pub callsign: String,
    /// Maidenhead grid locator (empty for type-2 messages).
    pub grid: String,
    /// Transmitted power in dBm.
    pub power_dbm: i32,
    /// Frequency drift in Hz/minute.
    pub drift: i32,
    /// Sync quality metric.
    pub sync_quality: f32,
    /// Number of decode passes required.
    pub npass: u8,
    /// OSD pass on which the decode succeeded.
    pub osd_pass: u8,
    /// Minimum hard-decision count across the decode.
    pub nhardmin: i32,
    /// Number of decoder iterations.
    pub decode_cycles: u32,
    /// Number of candidate messages explored during decode.
    pub candidates: u32,
    /// Fano metric for the decoded path.
    pub nfano: i32,
}

impl WsprSpotRow {
    /// Construct a [`WsprSpotRow`] from a [`WsprSpot`] and the window's
    /// [`SystemTime`] boundary.
    ///
    /// # Arguments
    ///
    /// * `spot`         — decoded WSPR spot
    /// * `window_start` — WSPR window boundary (even UTC minute)
    pub fn from_spot(spot: &WsprSpot, window_start: SystemTime) -> Self {
        let window_start_unix = window_start
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs() as i64;
        Self {
            window_start_unix,
            time_utc: spot.time_utc.clone(),
            snr_db: spot.snr_db,
            dt_sec: spot.dt_sec,
            freq_hz: spot.freq_hz,
            message: spot.message.clone(),
            callsign: spot.callsign.clone(),
            grid: spot.grid.clone(),
            power_dbm: spot.power_dbm,
            drift: spot.drift,
            sync_quality: spot.sync_quality,
            npass: spot.npass,
            osd_pass: spot.osd_pass,
            nhardmin: spot.nhardmin,
            decode_cycles: spot.decode_cycles,
            candidates: spot.candidates,
            nfano: spot.nfano,
        }
    }
}
