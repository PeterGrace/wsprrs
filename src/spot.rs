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
