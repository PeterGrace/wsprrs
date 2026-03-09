/// WSPR decoding: write `.wav` files, manage FFTW wisdom, invoke `wsprd`,
/// and parse the richer `ALL_WSPR.TXT` output.
///
/// ka9q-radio in USB mode outputs mono 16-bit PCM audio at 12 kHz.
/// The WSPR signal sits in the 1300–1700 Hz audio passband (centre ≈ 1500 Hz).
///
/// Each decode runs in its own temporary subdirectory so that concurrent
/// `wsprd` processes write their `ALL_WSPR.TXT` and `.c2` files without
/// colliding with each other.
///
/// ## FFTW wisdom
///
/// `wsprd` uses FFTW internally and will save a `wspr_wisdom.dat` file in its
/// working directory after computing the optimal FFT plan for the host CPU.
/// We copy this file into each temp subdirectory before running `wsprd` (if
/// we have one) and copy it back out after the first successful run (if we
/// do not yet have one).  Subsequent decodes benefit from skipping FFT planning.
///
/// ## `ALL_WSPR.TXT` format (wsprd 2.5+)
///
/// ```text
/// <rx_id> <hash> <snr> <dt> <freq_MHz> <call> [<grid>] <pwr>
///     <drift> <sync_q> <npass> <osd_pass> <nhardmin> <nbadcrc>
///     <ncycles> <candidates> <nfano>
/// ```
///
/// Grid is a 4- or 6-character Maidenhead locator and is absent for type-2
/// WSPR messages (e.g. `XDU/B4LHM 13`).  `<...>` is used as the callsign
/// placeholder in type-3 hash messages paired with a 6-char grid.
use std::io::Write as IoWrite;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::buffer::AudioWindow;
use crate::error::WsprError;
use crate::spot::WsprSpot;

// ─── WAV I/O ─────────────────────────────────────────────────────────────────

/// Write a standard RIFF/WAV file to `path` for the given audio window.
///
/// The WAV contains 16-bit signed PCM, mono, at `window.sample_rate_hz`.
/// This function is synchronous and intended to be called inside
/// [`tokio::task::spawn_blocking`].
///
/// # Arguments
///
/// * `window` — sealed audio window containing the PCM samples
/// * `path`   — destination file path (must not already exist)
///
/// # Errors
///
/// Returns any I/O error encountered while writing.
pub fn write_wav_file(window: &AudioWindow, path: &Path) -> Result<()> {
    // Use a NamedTempFile in the same directory to get safe atomic placement,
    // then persist it to `path`.
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = tempfile::Builder::new()
        .prefix(".wsprrs_wav_")
        .tempfile_in(dir)
        .context("failed to create wav tempfile")?;
    let mut f = tmp.reopen().context("failed to reopen wav tempfile")?;

    let num_channels: u16 = 1;
    let bits_per_sample: u16 = 16;
    let sample_rate = window.sample_rate_hz;
    let byte_rate: u32 = sample_rate * num_channels as u32 * (bits_per_sample / 8) as u32;
    let block_align: u16 = num_channels * (bits_per_sample / 8);
    let data_size: u32 =
        window.capacity_samples as u32 * num_channels as u32 * (bits_per_sample / 8) as u32;
    let riff_size: u32 = 4 + 8 + 16 + 8 + data_size;

    f.write_all(b"RIFF").context("write RIFF")?;
    f.write_all(&riff_size.to_le_bytes()).context("write riff_size")?;
    f.write_all(b"WAVE").context("write WAVE")?;
    f.write_all(b"fmt ").context("write fmt tag")?;
    f.write_all(&16u32.to_le_bytes()).context("write fmt size")?;
    f.write_all(&1u16.to_le_bytes()).context("write audio_format")?;
    f.write_all(&num_channels.to_le_bytes()).context("write num_channels")?;
    f.write_all(&sample_rate.to_le_bytes()).context("write sample_rate")?;
    f.write_all(&byte_rate.to_le_bytes()).context("write byte_rate")?;
    f.write_all(&block_align.to_le_bytes()).context("write block_align")?;
    f.write_all(&bits_per_sample.to_le_bytes()).context("write bits_per_sample")?;
    f.write_all(b"data").context("write data tag")?;
    f.write_all(&data_size.to_le_bytes()).context("write data_size")?;

    // ka9q-radio delivers S16BE; we decoded to native i16 in ingest(), so
    // write each value as LE bytes for the standard WAV format.
    for &sample in window.samples() {
        f.write_all(&sample.to_le_bytes()).context("write sample")?;
    }
    f.flush().context("flush wav")?;

    // Atomically place the file at the destination path.
    tmp.persist(path).context("persist wav tempfile")?;
    Ok(())
}

// ─── FFTW wisdom ─────────────────────────────────────────────────────────────

/// Copy `wspr_wisdom.dat` from `wisdom_path` into `work_dir`, if available.
///
/// Silently skips if `wisdom_path` does not exist (expected on the first run).
/// Logs a warning for other I/O errors but never returns an error — a missing
/// wisdom file is a performance issue, not a correctness issue.
pub async fn copy_wisdom_in(wisdom_path: &Path, work_dir: &Path) {
    let dest = work_dir.join("wspr_wisdom.dat");
    match tokio::fs::copy(wisdom_path, &dest).await {
        Ok(_) => tracing::debug!(
            src = %wisdom_path.display(),
            "copied wspr_wisdom.dat into decode work dir"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!("wspr_wisdom.dat not yet available; wsprd will create one");
        }
        Err(e) => tracing::warn!(
            error = %e,
            wisdom = %wisdom_path.display(),
            "could not copy wspr_wisdom.dat into decode work dir"
        ),
    }
}

/// Copy `wspr_wisdom.dat` from `work_dir` back to `wisdom_path`, if not
/// already present.
///
/// Uses a write-then-rename sequence so concurrent calls (from parallel
/// decode tasks that all finished without a pre-existing wisdom file) are
/// safe: the rename is atomic on Linux/macOS, and all tasks would be writing
/// identical content (FFTW wisdom is deterministic for a given CPU).
pub async fn copy_wisdom_out(work_dir: &Path, wisdom_path: &Path) {
    let src = work_dir.join("wspr_wisdom.dat");

    // Cheap stat check — no need for async here.
    if !src.exists() {
        tracing::debug!("wsprd did not produce wspr_wisdom.dat in work dir");
        return;
    }
    if wisdom_path.exists() {
        return; // already have one; nothing to do
    }

    // Write to a sibling temp file then rename → atomic placement.
    let tmp_path = wisdom_path.with_extension("dat.tmp");
    if let Err(e) = tokio::fs::copy(&src, &tmp_path).await {
        tracing::warn!(error = %e, "failed to stage wspr_wisdom.dat.tmp");
        return;
    }
    match tokio::fs::rename(&tmp_path, wisdom_path).await {
        Ok(()) => tracing::info!(
            path = %wisdom_path.display(),
            "saved wspr_wisdom.dat for future runs"
        ),
        Err(e) => tracing::warn!(error = %e, "failed to install wspr_wisdom.dat"),
    }
}

// ─── wsprd subprocess ────────────────────────────────────────────────────────

/// Run `wsprd` in `work_dir` and return the decoded spots from `ALL_WSPR.TXT`.
///
/// `wsprd` writes `ALL_WSPR.TXT` (and optional `.c2` files) to its working
/// directory.  By running each decode in an isolated temp subdirectory these
/// files never collide between concurrent decodes.
///
/// # Arguments
///
/// * `wsprd_path`   — path or name of the `wsprd` binary
/// * `wav_path`     — absolute path to the `.wav` file to decode
/// * `dial_freq_hz` — USB dial/carrier frequency in Hz (from the status stream)
/// * `window_start` — window boundary timestamp; used to derive `time_utc`
/// * `work_dir`     — working directory; `ALL_WSPR.TXT` is read from here
///
/// # Errors
///
/// * [`WsprError::Io`] if the subprocess cannot be spawned
/// * [`WsprError::WsprdFailed`] if `wsprd` exits non-zero
pub async fn run_wsprd(
    wsprd_path: &str,
    wav_path: &Path,
    dial_freq_hz: f64,
    window_start: SystemTime,
    work_dir: &Path,
) -> Result<Vec<WsprSpot>, WsprError> {
    let dial_freq_mhz = dial_freq_hz / 1_000_000.0;

    let output = Command::new(wsprd_path)
        .arg("-f")
        .arg(format!("{dial_freq_mhz:.6}"))
        .arg(wav_path)
        .current_dir(work_dir)
        .output()
        .await
        .map_err(WsprError::Io)?;

    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(WsprError::WsprdFailed { code, stderr });
    }

    // ALL_WSPR.TXT is only created when at least one spot is decoded.
    let all_wspr_path = work_dir.join("ALL_WSPR.TXT");
    let content = match tokio::fs::read_to_string(&all_wspr_path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(WsprError::Io(e)),
    };

    Ok(parse_all_wspr_txt(&content, window_start))
}

// ─── ALL_WSPR.TXT parsing ────────────────────────────────────────────────────

/// Return `true` if `s` is a valid 4- or 6-character Maidenhead grid locator.
///
/// * **4-char**: `[A-R][A-R][0-9][0-9]`  (e.g. `FN20`)
/// * **6-char**: above + `[A-X][A-X]`    (e.g. `FN20EJ` or `FN20ej`)
///
/// The trailing subsquare pair is case-insensitive; wsprd may emit uppercase.
fn is_grid_square(s: &str) -> bool {
    let b = s.as_bytes();
    let valid_major = |i: usize| (b'A'..=b'R').contains(&b[i].to_ascii_uppercase());
    let valid_minor = |i: usize| (b'A'..=b'X').contains(&b[i].to_ascii_uppercase());
    match b.len() {
        4 => valid_major(0) && valid_major(1) && b[2].is_ascii_digit() && b[3].is_ascii_digit(),
        6 => {
            valid_major(0)
                && valid_major(1)
                && b[2].is_ascii_digit()
                && b[3].is_ascii_digit()
                && valid_minor(4)
                && valid_minor(5)
        }
        _ => false,
    }
}

/// Format a [`SystemTime`] as a 4-digit `HHMM` UTC string.
fn hhmm_utc(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs();
    let hh = (secs % 86400) / 3600;
    let mm = (secs % 3600) / 60;
    format!("{hh:02}{mm:02}")
}

/// Parse all lines from `ALL_WSPR.TXT`, returning one [`WsprSpot`] per
/// successfully decoded line.  Unparseable lines are logged and skipped.
fn parse_all_wspr_txt(content: &str, window_start: SystemTime) -> Vec<WsprSpot> {
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| match parse_all_wspr_line(line.trim(), window_start) {
            Ok(spot) => Some(spot),
            Err(WsprError::SpotParseFailed(ref s)) => {
                tracing::warn!(line = s.as_str(), "skipping unparseable ALL_WSPR.TXT line");
                None
            }
            Err(e) => {
                tracing::error!(error = %e, "unexpected error parsing ALL_WSPR.TXT line");
                None
            }
        })
        .collect()
}

/// Parse a single `ALL_WSPR.TXT` line.
///
/// Column layout (wsprd 2.5+):
///
/// ```text
/// [0] rx_id       (discarded)
/// [1] hash        (discarded)
/// [2] snr
/// [3] dt
/// [4] freq_MHz
/// [5] call
/// [6] grid        (present for type-1 / type-3; absent for type-2)
/// [?] pwr
/// [?] drift
/// [?] sync_quality
/// [?] npass
/// [?] osd_pass
/// [?] nhardmin
/// [?] nbadcrc     (discarded — always 0 for a valid CRC decode)
/// [?] ncycles     → decode_cycles
/// [?] candidates
/// [?] nfano
/// ```
///
/// Whether field[6] is a grid or the power value is detected by
/// [`is_grid_square`].
fn parse_all_wspr_line(line: &str, window_start: SystemTime) -> Result<WsprSpot, WsprError> {
    let f: Vec<&str> = line.split_whitespace().collect();
    let err = || WsprError::SpotParseFailed(line.to_owned());

    // Minimum needed: rx_id hash snr dt freq call pwr drift sync npass osd
    //                 nhardmin nbadcrc ncycles candidates nfano  = 16 fields (no grid)
    if f.len() < 16 {
        return Err(err());
    }

    let snr_db: i32 = f[2].parse().map_err(|_| err())?;
    let dt_sec: f32 = f[3].parse().map_err(|_| err())?;
    let freq_mhz: f64 = f[4].parse().map_err(|_| err())?;
    let callsign = f[5].to_owned();

    // Field[6] is either a grid square or the power value.
    // `base` is the index of the power field.
    let (grid, base) = if is_grid_square(f[6]) {
        (f[6].to_owned(), 7usize)
    } else {
        (String::new(), 6usize)
    };

    // Need base + 10 fields: pwr drift sync npass osd nhardmin nbadcrc ncycles candidates nfano
    if f.len() < base + 10 {
        return Err(err());
    }

    let power_dbm: i32 = f[base].parse().map_err(|_| err())?;
    let drift: i32 = f[base + 1].parse().map_err(|_| err())?;
    let sync_quality: f32 = f[base + 2].parse().map_err(|_| err())?;
    let npass: u8 = f[base + 3].parse().map_err(|_| err())?;
    let osd_pass: u8 = f[base + 4].parse().map_err(|_| err())?;
    let nhardmin: i32 = f[base + 5].parse().map_err(|_| err())?;
    // f[base + 6] is nbadcrc — always 0 for a valid decode; we skip it.
    let decode_cycles: u32 = f[base + 7].parse().map_err(|_| err())?;
    let candidates: u32 = f[base + 8].parse().map_err(|_| err())?;
    let nfano: i32 = f[base + 9].parse().map_err(|_| err())?;

    let message = if grid.is_empty() {
        format!("{callsign} {power_dbm}")
    } else {
        format!("{callsign} {grid} {power_dbm}")
    };

    Ok(WsprSpot {
        time_utc: hhmm_utc(window_start),
        snr_db,
        dt_sec,
        freq_hz: freq_mhz * 1_000_000.0,
        message,
        grid,
        power_dbm,
        drift,
        sync_quality,
        npass,
        osd_pass,
        nhardmin,
        decode_cycles,
        candidates,
        nfano,
    })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::AudioWindow;
    use std::time::UNIX_EPOCH;

    // ---- is_grid_square ----

    #[test]
    fn grid_4char_valid() {
        assert!(is_grid_square("FN20"));
        assert!(is_grid_square("EM73"));
        assert!(is_grid_square("AR00")); // edge: A, R, 0, 0
    }

    #[test]
    fn grid_6char_valid() {
        assert!(is_grid_square("FN20EJ")); // uppercase subsquare
        assert!(is_grid_square("EL49WX"));
        assert!(is_grid_square("FN20ej")); // lowercase subsquare
    }

    #[test]
    fn grid_invalid_cases() {
        assert!(!is_grid_square("53"));      // power value
        assert!(!is_grid_square("<...>"));   // hash callsign
        assert!(!is_grid_square("ZZ99"));    // Z not in A-R
        assert!(!is_grid_square("FN2"));     // too short
        assert!(!is_grid_square("FN20EJX")); // too long
    }

    // ---- hhmm_utc ----

    #[test]
    fn hhmm_utc_unix_epoch() {
        assert_eq!(hhmm_utc(UNIX_EPOCH), "0000");
    }

    #[test]
    fn hhmm_utc_specific_time() {
        // 2024-01-01 01:02:00 UTC = 1704070920s
        let t = UNIX_EPOCH + Duration::from_secs(1_704_070_920);
        assert_eq!(hhmm_utc(t), "0102");
    }

    // ---- parse_all_wspr_line ----

    #[test]
    fn parse_type1_with_4char_grid() {
        let line =
            "prrs_G NsUf -26  0.58   5.3661911  WA5DXP EL49 37   0  0.20  1  1    0  0  28     5    13";
        let spot = parse_all_wspr_line(line, UNIX_EPOCH).expect("parse failed");

        assert_eq!(spot.snr_db, -26);
        assert!((spot.dt_sec - 0.58).abs() < 1e-3);
        assert!((spot.freq_hz - 5_366_191.1).abs() < 1.0);
        assert_eq!(spot.grid, "EL49");
        assert_eq!(spot.power_dbm, 37);
        assert_eq!(spot.drift, 0);
        assert!((spot.sync_quality - 0.20).abs() < 1e-3);
        assert_eq!(spot.npass, 1);
        assert_eq!(spot.osd_pass, 1);
        assert_eq!(spot.nhardmin, 0);
        assert_eq!(spot.decode_cycles, 28);
        assert_eq!(spot.candidates, 5);
        assert_eq!(spot.nfano, 13);
        assert_eq!(spot.message, "WA5DXP EL49 37");
    }

    #[test]
    fn parse_type3_with_6char_grid_and_hash_callsign() {
        let line =
            "prrs_k Pu8G -26  0.15   3.5700889  <...> FN20EJ 37  0  0.22  1  1    0  0  65   280   -77";
        let spot = parse_all_wspr_line(line, UNIX_EPOCH).expect("parse failed");

        assert_eq!(spot.grid, "FN20EJ");
        assert_eq!(spot.power_dbm, 37);
        assert_eq!(spot.candidates, 280);
        assert_eq!(spot.nfano, -77);
        assert_eq!(spot.message, "<...> FN20EJ 37");
    }

    #[test]
    fn parse_type2_no_grid() {
        // Type-2 message: callsign/prefix + power, no grid.
        let line =
            "prrs_t 0aYF  23 -0.36   7.0400808  1HO/VV8XYH 53   2  0.18  2  1   56  0  73  7050  -235";
        let spot = parse_all_wspr_line(line, UNIX_EPOCH).expect("parse failed");

        assert_eq!(spot.grid, "");
        assert_eq!(spot.power_dbm, 53);
        assert_eq!(spot.drift, 2);
        assert_eq!(spot.nhardmin, 56);
        assert_eq!(spot.candidates, 7050);
        assert_eq!(spot.nfano, -235);
        assert_eq!(spot.message, "1HO/VV8XYH 53");
    }

    #[test]
    fn parse_negative_nhardmin_and_drift() {
        let line =
            "prrs_t SdCR -31  0.45   3.5700002  KF9KV EN52 17   0  0.16  3  3   -8  0  30  3737  -178";
        let spot = parse_all_wspr_line(line, UNIX_EPOCH).expect("parse failed");

        assert_eq!(spot.snr_db, -31);
        assert_eq!(spot.nhardmin, -8);
        assert_eq!(spot.npass, 3);
        assert_eq!(spot.osd_pass, 3);
        assert_eq!(spot.decode_cycles, 30);
        assert_eq!(spot.candidates, 3737);
        assert_eq!(spot.nfano, -178);
    }

    #[test]
    fn parse_derives_time_from_window_start() {
        let t = UNIX_EPOCH + Duration::from_secs(1_704_070_920); // 01:02 UTC
        let line =
            "prrs_G NsUf -26  0.58   5.3661911  WA5DXP EL49 37   0  0.20  1  1    0  0  28     5    13";
        let spot = parse_all_wspr_line(line, t).expect("parse failed");
        assert_eq!(spot.time_utc, "0102");
    }

    #[test]
    fn parse_too_short_returns_error() {
        let result = parse_all_wspr_line("too short", UNIX_EPOCH);
        assert!(matches!(result, Err(WsprError::SpotParseFailed(_))));
    }

    // ---- parse_all_wspr_txt ----

    #[test]
    fn parse_full_output_multi_line() {
        let content = "\
prrs_G NsUf -26  0.58   5.3661911  WA5DXP EL49 37   0  0.20  1  1    0  0  28     5    13
prrs_t SdCR -26  0.66   3.5700455  N0QBH EN25 37    0  0.25  1  1    0  0  17    11   225

prrs_t 0aYF  23 -0.36   7.0400808  1HO/VV8XYH 53   2  0.18  2  1   56  0  73  7050  -235
";
        let spots = parse_all_wspr_txt(content, UNIX_EPOCH);
        assert_eq!(spots.len(), 3);
        assert_eq!(spots[0].grid, "EL49");
        assert_eq!(spots[1].grid, "EN25");
        assert_eq!(spots[2].grid, "");
    }

    #[test]
    fn parse_empty_file_returns_no_spots() {
        assert!(parse_all_wspr_txt("", UNIX_EPOCH).is_empty());
        assert!(parse_all_wspr_txt("\n\n", UNIX_EPOCH).is_empty());
    }

    // ---- write_wav_file ----

    #[test]
    fn wav_file_is_written() {
        let window = AudioWindow::new(UNIX_EPOCH, 14_095_600.0, 12_000, 116);
        let dir = tempfile::tempdir().expect("tempdir failed");
        let path = dir.path().join("test.wav");
        write_wav_file(&window, &path).expect("write_wav_file failed");
        let meta = std::fs::metadata(&path).expect("stat failed");
        let expected = 44 + 12_000usize * 116 * 2; // 44-byte header + PCM data
        assert_eq!(meta.len() as usize, expected);
    }
}
