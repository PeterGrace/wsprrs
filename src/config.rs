use anyhow::{Context, Result};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs as _};
use std::path::PathBuf;

/// Resolve a string to an [`IpAddr`], accepting either a literal IP address
/// or a hostname (including `.local` mDNS names via the OS resolver / Avahi).
///
/// # How it works
///
/// 1. Fast path: try to parse `s` directly as an IP address literal — no
///    syscall needed.
/// 2. Slow path: call the OS `getaddrinfo(3)` resolver via
///    [`ToSocketAddrs`](std::net::ToSocketAddrs).  On Linux with `nss-mdns`
///    installed, this transparently resolves `.local` Avahi-registered names.
///
/// # Errors
///
/// Returns an error if the string is neither a valid IP literal nor a name
/// that the OS resolver can resolve, or if the name resolves to no addresses.
fn resolve_host(s: &str) -> Result<IpAddr> {
    // Fast path: plain IP literal ("239.1.2.3", "::1", etc.)
    if let Ok(ip) = s.parse::<IpAddr>() {
        return Ok(ip);
    }

    // Slow path: hand off to the OS resolver.  `ToSocketAddrs` requires a
    // port in the tuple, but we only need the IP — port 0 is a harmless
    // placeholder.
    (s, 0u16)
        .to_socket_addrs()
        .with_context(|| format!("could not resolve host {:?}", s))?
        .map(|sa| sa.ip())
        .next()
        .with_context(|| format!("{:?} resolved to no addresses", s))
}

/// Runtime configuration loaded from environment / `.env`.
///
/// Call [`Config::from_env`] after `dotenvy::dotenv()` has been invoked.
///
/// Centre frequency and sample rate are **not** configured here — they are
/// delivered per-SSRC via the ka9q-radio status stream (port `status_port`).
#[derive(Debug, Clone)]
pub struct Config {
    /// Multicast group address (e.g. `239.1.2.3`).
    pub multicast_addr: IpAddr,

    /// UDP port for the RTP data stream (e.g. `5004`).
    pub multicast_port: u16,

    /// UDP port for the ka9q-radio status stream (e.g. `5006`).
    pub status_port: u16,

    /// Local interface address to bind and join the multicast group on.
    /// Defaults to `0.0.0.0` (let the OS choose).
    pub local_addr: IpAddr,

    /// Optional SSRC filter.  When `None`, all stereo-IQ SSRCs are tracked.
    pub ssrc_filter: Option<u32>,

    /// Number of seconds to capture per WSPR window.  Must be >= 111.
    /// Defaults to `116`.
    pub capture_seconds: u32,

    /// Directory for temporary WAV files.  Defaults to `/tmp`.
    pub temp_dir: String,

    /// Path or name of the `wsprd` binary.  Defaults to `"wsprd"`.
    pub wsprd_path: String,

    /// Optional path to an output file for decoded WSPR spots.
    /// Each spot is appended as a single JSON line (NDJSON).
    /// When `None`, spots are only emitted via tracing.
    pub output_file: Option<String>,

    /// Path to the FFTW wisdom file used by `wsprd` to skip FFT planning.
    ///
    /// If the file exists it is copied into each per-decode temp directory
    /// before `wsprd` runs.  If it does not exist the first successful decode
    /// will copy one back here so subsequent runs benefit from it.
    ///
    /// Defaults to `wspr_wisdom.dat` in the current working directory.
    pub wisdom_file: PathBuf,
}

impl Config {
    /// Load configuration from environment variables.
    ///
    /// # Required variables
    /// * `WSPR_MULTICAST_ADDR`  — multicast group IP or resolvable hostname (including `.local` mDNS)
    /// * `WSPR_MULTICAST_PORT`  — UDP port for the RTP data stream
    ///
    /// # Optional variables
    /// * `WSPR_STATUS_PORT`     — UDP port for the status stream (default `5006`)
    /// * `WSPR_LOCAL_ADDR`      — interface address (default `0.0.0.0`)
    /// * `WSPR_SSRC`            — hex SSRC filter (default: all stereo-IQ SSRCs)
    /// * `WSPR_CAPTURE_SECONDS` — capture window length in seconds (default `116`)
    /// * `WSPR_TEMP_DIR`        — temp file directory (default `/tmp`)
    /// * `WSPR_WSPRD_PATH`      — path to `wsprd` binary (default `wsprd`)
    /// * `WSPR_OUTPUT_FILE`     — path to NDJSON spot log (default: none)
    /// * `WSPR_WISDOM_FILE`     — path to FFTW wisdom file (default `wspr_wisdom.dat`)
    ///
    /// # Errors
    ///
    /// Returns an error if any required variable is missing or any value
    /// cannot be parsed.
    pub fn from_env() -> Result<Self> {
        let multicast_addr = resolve_host(
            &std::env::var("WSPR_MULTICAST_ADDR").context("WSPR_MULTICAST_ADDR not set")?,
        )
        .context("WSPR_MULTICAST_ADDR is not a valid IP address or resolvable hostname")?;

        let multicast_port: u16 = std::env::var("WSPR_MULTICAST_PORT")
            .context("WSPR_MULTICAST_PORT not set")?
            .parse()
            .context("WSPR_MULTICAST_PORT is not a valid port number")?;

        let status_port: u16 = std::env::var("WSPR_STATUS_PORT")
            .unwrap_or_else(|_| "5006".to_string())
            .parse()
            .context("WSPR_STATUS_PORT is not a valid port number")?;

        let local_addr: IpAddr = std::env::var("WSPR_LOCAL_ADDR")
            .unwrap_or_else(|_| "0.0.0.0".to_string())
            .parse()
            .context("WSPR_LOCAL_ADDR is not a valid IP address")?;

        let ssrc_filter: Option<u32> = match std::env::var("WSPR_SSRC") {
            Ok(s) => {
                // Accept with or without 0x prefix.
                let hex = s.trim_start_matches("0x").trim_start_matches("0X");
                let v = u32::from_str_radix(hex, 16).context("WSPR_SSRC is not a valid hex u32")?;
                Some(v)
            }
            Err(_) => None,
        };

        let capture_seconds: u32 = std::env::var("WSPR_CAPTURE_SECONDS")
            .unwrap_or_else(|_| "116".to_string())
            .parse()
            .context("WSPR_CAPTURE_SECONDS is not a valid u32")?;

        anyhow::ensure!(
            capture_seconds >= 111,
            "WSPR_CAPTURE_SECONDS must be >= 111 (got {capture_seconds})"
        );

        let temp_dir = std::env::var("WSPR_TEMP_DIR").unwrap_or_else(|_| "/tmp".to_string());
        let wsprd_path = std::env::var("WSPR_WSPRD_PATH").unwrap_or_else(|_| "wsprd".to_string());
        let output_file = std::env::var("WSPR_OUTPUT_FILE").ok();
        let wisdom_file = std::env::var("WSPR_WISDOM_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("wspr_wisdom.dat"));

        Ok(Self {
            multicast_addr,
            multicast_port,
            status_port,
            local_addr,
            ssrc_filter,
            capture_seconds,
            temp_dir,
            wsprd_path,
            output_file,
            wisdom_file,
        })
    }

    /// Convenience: return the socket address for binding the data UDP socket.
    ///
    /// Not used by the current multicast join path but kept as a public API
    /// convenience for callers that need it.
    #[allow(dead_code)]
    pub fn bind_addr(&self) -> SocketAddr {
        SocketAddr::new(self.local_addr, self.multicast_port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Process-global mutex serialising all config env-var tests.
    ///
    /// Rust test threads run in parallel within a single process; environment
    /// variables are process-global, so concurrent mutations are a data race.
    /// Holding this lock for the entire duration of each test prevents
    /// interference between tests without requiring `--test-threads=1`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Clear every env var read by `from_env`, then set the given ones.
    ///
    /// Must be called while `ENV_LOCK` is held.
    fn set_env(vars: &[(&str, &str)]) {
        for name in &[
            "WSPR_MULTICAST_ADDR",
            "WSPR_MULTICAST_PORT",
            "WSPR_STATUS_PORT",
            "WSPR_LOCAL_ADDR",
            "WSPR_SSRC",
            "WSPR_CAPTURE_SECONDS",
            "WSPR_TEMP_DIR",
            "WSPR_WSPRD_PATH",
            "WSPR_OUTPUT_FILE",
            "WSPR_WISDOM_FILE",
        ] {
            std::env::remove_var(name);
        }
        for &(k, v) in vars {
            std::env::set_var(k, v);
        }
    }

    #[test]
    fn from_env_parses_required_vars() {
        // Arrange: hold the lock for the entire test to prevent parallel tests
        // from mutating env vars beneath us.
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        set_env(&[
            ("WSPR_MULTICAST_ADDR", "239.1.2.3"),
            ("WSPR_MULTICAST_PORT", "5004"),
        ]);

        // Act
        let cfg = Config::from_env().expect("Config::from_env failed");

        // Assert
        assert_eq!(cfg.multicast_port, 5004);
        assert_eq!(cfg.status_port, 5006);
        assert_eq!(cfg.capture_seconds, 116);
        assert!(cfg.ssrc_filter.is_none());
        assert_eq!(cfg.wsprd_path, "wsprd");
    }

    #[test]
    fn from_env_parses_ssrc_hex() {
        // Arrange
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        set_env(&[
            ("WSPR_MULTICAST_ADDR", "239.1.2.3"),
            ("WSPR_MULTICAST_PORT", "5004"),
            ("WSPR_SSRC", "0xDEADBEEF"),
        ]);

        // Act
        let cfg = Config::from_env().expect("Config::from_env failed");

        // Assert
        assert_eq!(cfg.ssrc_filter, Some(0xDEAD_BEEF));
    }

    #[test]
    fn from_env_parses_custom_status_port() {
        // Arrange
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        set_env(&[
            ("WSPR_MULTICAST_ADDR", "239.1.2.3"),
            ("WSPR_MULTICAST_PORT", "5004"),
            ("WSPR_STATUS_PORT", "5010"),
        ]);

        // Act
        let cfg = Config::from_env().expect("Config::from_env failed");

        // Assert
        assert_eq!(cfg.status_port, 5010);
    }

    #[test]
    fn from_env_rejects_short_capture() {
        // Arrange
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        set_env(&[
            ("WSPR_MULTICAST_ADDR", "239.1.2.3"),
            ("WSPR_MULTICAST_PORT", "5004"),
            ("WSPR_CAPTURE_SECONDS", "50"),
        ]);

        // Act
        let result = Config::from_env();

        // Assert
        assert!(result.is_err(), "expected error for capture_seconds < 111");
    }

    // ── resolve_host unit tests ────────────────────────────────────────────

    #[test]
    fn resolve_host_accepts_ipv4_literal() {
        let ip = resolve_host("239.1.2.3").expect("should parse IPv4 literal");
        assert_eq!(ip.to_string(), "239.1.2.3");
    }

    #[test]
    fn resolve_host_accepts_ipv6_literal() {
        let ip = resolve_host("::1").expect("should parse IPv6 literal");
        assert!(ip.is_loopback());
    }

    #[test]
    fn resolve_host_resolves_localhost() {
        // "localhost" must resolve on every reasonable OS; confirms the
        // getaddrinfo code path works.
        let ip = resolve_host("localhost").expect("localhost should resolve");
        assert!(ip.is_loopback(), "localhost should be a loopback address, got {ip}");
    }

    #[test]
    fn resolve_host_rejects_gibberish() {
        // A hostname that cannot possibly exist should yield an error.
        let result = resolve_host("this.hostname.does.not.exist.invalid");
        assert!(result.is_err(), "expected resolution failure for nonexistent hostname");
    }
}
