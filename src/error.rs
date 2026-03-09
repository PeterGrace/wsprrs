/// Application-level error type for wsprrs.
///
/// Each variant maps to a specific failure domain, making it easy to
/// distinguish network errors from parsing errors from subprocess failures.
#[derive(Debug, thiserror::Error)]
pub enum WsprError {
    /// RTP packet was shorter than the 12-byte fixed header, or a CSRC/extension
    /// block overran the buffer.
    #[error("RTP packet too short: {0} bytes")]
    PacketTooShort(usize),

    /// RTP version field was not 2.
    #[error("unsupported RTP version: {0}")]
    UnsupportedRtpVersion(u8),

    /// More samples arrived than the pre-allocated window can hold.
    #[error("IQ buffer overflow for SSRC 0x{ssrc:08x}")]
    BufferOverflow { ssrc: u32 },

    /// Any std::io::Error (socket, file, process I/O).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// `wsprd` exited with a non-zero status.
    #[error("`wsprd` failed with exit code {code}: {stderr}")]
    WsprdFailed { code: i32, stderr: String },

    /// A line of `wsprd` output did not match the expected format.
    #[error("failed to parse wsprd output line: {0:?}")]
    SpotParseFailed(String),
}
