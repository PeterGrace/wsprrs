/// wsprrs — ka9q-radio WSPR decoder
///
/// Receives IQ data from a running `ka9q-radio` instance over multicast
/// UDP/RTP, buffers it into 2-minute windows aligned to even UTC minutes,
/// decodes spots by calling `wsprd` as a subprocess, and logs decoded spots
/// to stdout as JSON.
///
/// # Configuration
///
/// All settings are loaded from environment variables (optionally via `.env`).
/// See [`config::Config::from_env`] for the full list.
///
/// # Architecture
///
/// Four concurrent Tokio tasks:
///
/// 1. **recv_task**   — reads RTP data UDP datagrams and sends them on a channel.
/// 2. **status_task** — reads ka9q-radio status UDP datagrams, parses per-SSRC
///    channel metadata (frequency, sample rate, encoding, PT), and updates a
///    shared [`HashMap`] under an [`RwLock`].
/// 3. **buffer_task** — parses RTP headers, consults the status map to filter
///    stereo IQ streams, ingests samples into per-SSRC [`IqWindow`]s, and seals
///    windows when they expire.
/// 4. **decode_task** — receives sealed windows and drives `wsprd`.
///
/// All tasks share a [`Notify`] for cooperative shutdown on Ctrl-C.
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{mpsc, Notify, RwLock};
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::{prelude::*, EnvFilter, Registry};

// ---- indicatif / tracing integration ----
//
// `tracing_subscriber::fmt` and `indicatif::MultiProgress` both write to
// stderr directly.  When they interleave, raw terminal writes from tracing
// corrupt the progress bar ANSI rendering.  We fix this by routing every
// tracing log event through `MultiProgress::println`, which suspends the
// bars, prints the line, then redraws — keeping the two in sync.

/// A `MakeWriter` that routes each tracing event through indicatif's
/// `MultiProgress::println` so log lines appear above progress bars cleanly.
#[derive(Clone)]
struct MultiProgressMakeWriter(Arc<MultiProgress>);

/// Per-event writer produced by [`MultiProgressMakeWriter`].
///
/// Buffers all `write()` calls for one event, then emits a single
/// `MultiProgress::println` call on [`Drop`].
struct MultiProgressEventWriter {
    multi: Arc<MultiProgress>,
    buf: Vec<u8>,
}

impl std::io::Write for MultiProgressEventWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            // `MultiProgress::println` appends its own newline, so strip ours.
            if let Ok(s) = std::str::from_utf8(&self.buf) {
                let _ = self.multi.println(s.trim_end_matches('\n'));
            }
            self.buf.clear();
        }
        Ok(())
    }
}

impl Drop for MultiProgressEventWriter {
    fn drop(&mut self) {
        use std::io::Write;
        let _ = self.flush();
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for MultiProgressMakeWriter {
    type Writer = MultiProgressEventWriter;

    fn make_writer(&'a self) -> Self::Writer {
        MultiProgressEventWriter { multi: self.0.clone(), buf: Vec::new() }
    }
}

mod buffer;
mod config;
mod decode;
mod error;
mod multicast;
mod rtp;
mod spot;
mod status;

use buffer::{even_minute_boundary, AudioWindow};
use config::Config;
use error::WsprError;
use multicast::ReceivedPacket;
use rtp::parse_rtp_packet;
use spot::WsprSpot;
use status::ChannelInfo;

/// Channel capacity for raw UDP datagrams (receive → buffer task).
const PACKET_CHAN_CAP: usize = 1_024;

/// Channel capacity for sealed IQ windows (buffer → decode task).
const WINDOW_CHAN_CAP: usize = 8;

#[tokio::main]
async fn main() -> Result<()> {
    // Load `.env` if present; ignore errors (file may not exist).
    let _ = dotenvy::dotenv();

    // ---- indicatif MultiProgress (shared with buffer_task and tracing) ----
    // Created here so the tracing writer can route through it before any
    // progress bars are added; MultiProgress::println works fine with 0 bars.
    let multi = Arc::new(MultiProgress::new());

    // ---- tracing / logging setup ----
    // console-subscriber enables `tokio-console` introspection.
    let console_layer = console_subscriber::spawn();
    let filter_layer = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let format_layer = tracing_subscriber::fmt::layer()
        .event_format(
            tracing_subscriber::fmt::format()
                .with_file(true)
                .with_thread_ids(true)
                .with_thread_names(true)
                .with_line_number(true),
        )
        // Route through indicatif so log lines do not corrupt progress bars.
        .with_writer(MultiProgressMakeWriter(multi.clone()))
        .with_span_events(FmtSpan::NONE);

    Registry::default()
        .with(console_layer)
        .with(filter_layer)
        .with(format_layer)
        .init();

    tracing::info!("wsprrs starting");

    // ---- config ----
    let cfg = Config::from_env().context("failed to load configuration")?;
    tracing::info!(
        multicast    = %cfg.multicast_addr,
        data_port    = cfg.multicast_port,
        status_port  = cfg.status_port,
        ssrc_filter  = ?cfg.ssrc_filter,
        "configuration loaded"
    );

    // ---- shared shutdown signal ----
    let shutdown = Arc::new(Notify::new());

    {
        let shutdown = shutdown.clone();
        // ctrlc uses a thread, not Tokio; notify the async side.
        ctrlc::set_handler(move || {
            tracing::info!("Ctrl-C received; shutting down");
            shutdown.notify_waiters();
        })
        .context("failed to install Ctrl-C handler")?;
    }

    // ---- channels ----
    let (pkt_tx, pkt_rx) = mpsc::channel::<ReceivedPacket>(PACKET_CHAN_CAP);
    let (win_tx, win_rx) = mpsc::channel::<(AudioWindow, Arc<Config>)>(WINDOW_CHAN_CAP);

    // ---- shared per-SSRC channel metadata (populated by status_task) ----
    let channel_map: Arc<RwLock<HashMap<u32, ChannelInfo>>> = Arc::new(RwLock::new(HashMap::new()));

    // ---- multicast sockets ----
    let data_socket = multicast::build_socket(&cfg, cfg.multicast_port)
        .context("failed to build RTP data socket")?;
    tracing::info!(
        "joined multicast group {}:{} (data)",
        cfg.multicast_addr,
        cfg.multicast_port
    );

    let status_socket =
        multicast::build_socket(&cfg, cfg.status_port).context("failed to build status socket")?;
    tracing::info!(
        "joined multicast group {}:{} (status)",
        cfg.multicast_addr,
        cfg.status_port
    );

    let cfg = Arc::new(cfg);

    // ---- spawn tasks ----
    let recv_shutdown = shutdown.clone();
    let status_shutdown = shutdown.clone();
    let buf_shutdown = shutdown.clone();
    let dec_shutdown = shutdown.clone();

    let recv_handle = tokio::spawn(multicast::receive_loop(data_socket, pkt_tx, recv_shutdown));
    let status_handle = tokio::spawn(status::receive_loop(
        status_socket,
        channel_map.clone(),
        status_shutdown,
    ));
    let buf_handle = tokio::spawn(buffer_task(
        pkt_rx,
        win_tx,
        cfg.clone(),
        channel_map,
        multi,
        buf_shutdown,
    ));
    let dec_handle = tokio::spawn(decode_task(win_rx, cfg.clone(), dec_shutdown));

    // Wait for all four tasks to complete (they each exit on shutdown).
    let (r1, r2, r3, r4) = tokio::join!(recv_handle, status_handle, buf_handle, dec_handle);
    r1.context("receive task panicked")?;
    r2.context("status task panicked")?;
    r3.context("buffer task panicked")?;
    r4.context("decode task panicked")?;

    tracing::info!("wsprrs stopped");
    Ok(())
}

/// Manage per-SSRC audio windows.
///
/// Parses incoming RTP packets, consults `channel_map` to determine whether
/// each SSRC is a mono audio stream (channels == 1) ready for ingestion,
/// routes samples into the appropriate [`AudioWindow`], and seals + forwards
/// windows when their capture period expires.
///
/// A 1-second tick checks for expired windows.
///
/// # Arguments
///
/// * `pkt_rx`     — receiver for raw UDP datagrams from the receive task
/// * `win_tx`     — sender for sealed windows to the decode task
/// * `cfg`        — shared runtime configuration
/// * `channel_map`— shared map of per-SSRC channel metadata (written by status task)
/// * `multi`      — shared `MultiProgress` (also used by the tracing writer in main)
/// * `shutdown`   — shared `Notify`; when triggered this function returns
async fn buffer_task(
    mut pkt_rx: mpsc::Receiver<ReceivedPacket>,
    win_tx: mpsc::Sender<(AudioWindow, Arc<Config>)>,
    cfg: Arc<Config>,
    channel_map: Arc<RwLock<HashMap<u32, ChannelInfo>>>,
    multi: Arc<MultiProgress>,
    shutdown: Arc<Notify>,
) {
    // One AudioWindow per SSRC.  For typical WSPR use there will be several
    // (one per band), but each is handled independently.
    let mut windows: HashMap<u32, (AudioWindow, ProgressBar)> = HashMap::new();

    // Tick every second to check for expired windows.
    let mut interval = tokio::time::interval(Duration::from_secs(1));

    let bar_style = ProgressStyle::with_template(
        "{prefix} [{bar:40.cyan/blue}] {pos}/{len} samples ({percent}%) {msg}",
    )
    .expect("progress bar template is valid")
    .progress_chars("=> ");

    loop {
        tokio::select! {
            biased;

            _ = shutdown.notified() => {
                tracing::info!("buffer_task: shutdown signal received");
                break;
            }

            _ = interval.tick() => {
                // Check whether any window has exceeded its capture duration.
                let now = SystemTime::now();
                let capture_dur = Duration::from_secs(cfg.capture_seconds as u64);

                // Collect SSRCs whose windows have expired.
                let expired: Vec<u32> = windows
                    .iter()
                    .filter_map(|(&ssrc, (w, _))| {
                        let elapsed = now.duration_since(w.window_start).unwrap_or(Duration::ZERO);
                        if elapsed >= capture_dur { Some(ssrc) } else { None }
                    })
                    .collect();

                for ssrc in expired {
                    if let Some((window, pb)) = windows.remove(&ssrc) {
                        pb.finish_with_message("sealing");
                        tracing::info!(
                            ssrc,
                            samples_written = window.samples_written,
                            gap_count       = window.gap_count,
                            "sealing audio window for decode"
                        );
                        if win_tx.send((window, cfg.clone())).await.is_err() {
                            tracing::error!("decode task channel closed unexpectedly");
                        }
                    }
                }
            }

            maybe_pkt = pkt_rx.recv() => {
                let pkt = match maybe_pkt {
                    Some(p) => p,
                    None    => { tracing::info!("packet channel closed"); break; }
                };

                // Parse the RTP fixed header — all PTs are accepted at this layer.
                let rtp = match parse_rtp_packet(&pkt.data[..pkt.len]) {
                    Ok(r)  => r,
                    Err(e) => {
                        tracing::warn!(error = %e, "dropping malformed RTP packet");
                        continue;
                    }
                };

                let ssrc = rtp.header.ssrc;

                // Apply optional SSRC filter.
                if let Some(filter) = cfg.ssrc_filter {
                    if ssrc != filter {
                        continue;
                    }
                }

                // Look up this SSRC in the status-derived channel map.
                // We process mono audio streams (channels == 1, pt == 122) —
                // the standard ka9q-radio WSPR output in USB mode.
                let (info_freq, info_rate, info_encoding) = {
                    let map = channel_map.read().await;
                    match map.get(&ssrc) {
                        Some(info) if info.is_audio_ready() => {
                            (info.center_freq_hz, info.sample_rate_hz, info.encoding)
                        }
                        Some(_) => {
                            // Channel known but not a ready mono audio stream — skip.
                            continue;
                        }
                        None => {
                            // Channel not yet seen in status stream — skip until we know.
                            tracing::trace!(
                                ssrc = format!("0x{ssrc:08x}"),
                                pt = rtp.header.payload_type,
                                "unknown SSRC; waiting for status stream"
                            );
                            continue;
                        }
                    }
                };

                // Get or create the window for this SSRC.
                //
                // Before creating a new window, verify that the incoming packet
                // actually fits within the current even-minute cycle.  If the
                // program started mid-window (or if we missed the start), the
                // packet's RTP timestamp could be near the END of the cycle and
                // would immediately overflow the buffer.  In that case we skip
                // the packet and wait for the NEXT even-minute boundary.
                let (window, pb) = match windows.entry(ssrc) {
                    Entry::Occupied(e) => e.into_mut(),
                    Entry::Vacant(e) => {
                        let now          = SystemTime::now();
                        let window_start = even_minute_boundary(now);

                        // Use wall-clock elapsed time to decide whether we are
                        // still within the capture window for this cycle.
                        // Avoids any dependency on the RTP clock origin.
                        let elapsed_secs = now
                            .duration_since(window_start)
                            .unwrap_or_default()
                            .as_secs();

                        if elapsed_secs >= cfg.capture_seconds as u64 {
                            // We are in the tail of the cycle (past capture_seconds).
                            // Discard until the next even-minute boundary.
                            tracing::debug!(
                                ssrc,
                                elapsed_secs,
                                capacity_secs = cfg.capture_seconds,
                                "late start: waiting for next even-minute WSPR window"
                            );
                            continue;
                        }

                        let w     = AudioWindow::new(window_start, info_freq, info_rate, cfg.capture_seconds);
                        let total = w.capacity_samples as u64;
                        let bar   = multi.add(
                            ProgressBar::new(total)
                                .with_style(bar_style.clone())
                                .with_prefix(format!(
                                    "SSRC 0x{ssrc:08x} ({:.4} MHz)",
                                    info_freq / 1e6
                                ))
                        );
                        tracing::info!(
                            ssrc,
                            freq_hz     = info_freq,
                            sample_rate = info_rate,
                            "new audio window opened"
                        );
                        e.insert((w, bar))
                    }
                };

                // Ingest audio samples using the encoding reported by the status stream.
                match window.ingest(
                    rtp.payload,
                    rtp.header.timestamp,
                    info_rate,
                    rtp.header.sequence,
                    ssrc,
                    info_encoding,
                ) {
                    Ok(()) => {
                        let written = window.samples_written as u64;
                        pb.set_position(written);
                        pb.set_message(format!("{:.1}%", window.fill_fraction() * 100.0));
                    }
                    Err(WsprError::BufferOverflow { .. }) => {
                        // The buffer is full: the window has been captured to
                        // capacity.  Seal it for decode exactly as the timer
                        // tick would do — do NOT discard it.
                        if let Some((window, pb)) = windows.remove(&ssrc) {
                            pb.finish_with_message("sealing");
                            tracing::info!(
                                ssrc,
                                samples_written = window.samples_written,
                                gap_count       = window.gap_count,
                                "buffer full: sealing audio window for decode"
                            );
                            if win_tx.send((window, cfg.clone())).await.is_err() {
                                tracing::error!("decode task channel closed unexpectedly");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, ssrc, "ingest error; dropping window");
                        windows.remove(&ssrc);
                    }
                }
            }
        }
    }
}

/// Write WAV, manage FFTW wisdom, and run `wsprd` for a single sealed window.
///
/// Each window gets its own temporary subdirectory so concurrent `wsprd`
/// processes never collide on `ALL_WSPR.TXT` or `.c2` output files.
/// Wisdom is copied in before the run and, if newly generated, copied back
/// out afterward for use by future runs.
///
/// # Arguments
///
/// * `window` — sealed [`AudioWindow`] to decode
/// * `cfg`    — runtime configuration (paths, temp dir, wisdom file, etc.)
///
/// # Errors
///
/// Returns any temp-dir creation, WAV-write, or `wsprd` subprocess error.
async fn decode_window(window: AudioWindow, cfg: &Config) -> Result<Vec<WsprSpot>> {
    let dial_freq_hz  = window.center_freq_hz;
    let temp_dir_root = cfg.temp_dir.clone();
    let wsprd_path    = cfg.wsprd_path.clone();
    let wisdom_path   = cfg.wisdom_file.clone();
    let window_start  = window.window_start;

    // Create an isolated temp subdirectory and write the WAV file inside it.
    // Both operations are blocking I/O — offload to the blocking thread pool.
    let (work_dir, wav_path) = tokio::task::spawn_blocking(move || -> Result<_> {
        let dir = tempfile::Builder::new()
            .prefix("wsprrs_")
            .tempdir_in(&temp_dir_root)
            .context("failed to create decode work dir")?;
        let wav = dir.path().join("wspr.wav");
        decode::write_wav_file(&window, &wav).context("write_wav_file failed")?;
        Ok((dir, wav))
    })
    .await
    .context("spawn_blocking panicked creating work dir / WAV")?
    .context("work dir / WAV setup failed")?;

    // Copy FFTW wisdom in (silent no-op on first run).
    decode::copy_wisdom_in(&wisdom_path, work_dir.path()).await;

    tracing::debug!(
        wav_path = %wav_path.display(),
        dial_freq_hz,
        work_dir = %work_dir.path().display(),
        "running wsprd"
    );

    // Run wsprd; spots are read from ALL_WSPR.TXT in work_dir.
    let spots =
        decode::run_wsprd(&wsprd_path, &wav_path, dial_freq_hz, window_start, work_dir.path())
            .await
            .context("wsprd failed")?;

    // If this was the first run (no pre-existing wisdom), save the newly
    // generated file so subsequent decodes skip FFT planning.
    decode::copy_wisdom_out(work_dir.path(), &wisdom_path).await;

    // `work_dir` dropped here → entire temp subdirectory deleted.
    Ok(spots)
}

/// Decode sealed audio windows using `wsprd` and log spots as JSON.
///
/// Each window is dispatched to its own [`tokio::spawn`]ed task so that all
/// bands decode concurrently while audio capture continues uninterrupted.
/// Spot results flow back through an unbounded channel and are written
/// serially to the output file (preserving single-writer file semantics).
///
/// # Arguments
///
/// * `win_rx`  — receiver for sealed windows from the buffer task
/// * `cfg`     — shared runtime configuration
/// * `shutdown`— shared `Notify`; when triggered this function returns
async fn decode_task(
    mut win_rx: mpsc::Receiver<(AudioWindow, Arc<Config>)>,
    cfg: Arc<Config>,
    shutdown: Arc<Notify>,
) {
    // Open the output file once for the lifetime of this task (append + create).
    let mut out_file: Option<tokio::io::BufWriter<tokio::fs::File>> =
        match cfg.output_file.as_deref() {
            None => None,
            Some(path) => {
                match tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .await
                {
                    Ok(f) => {
                        tracing::info!(path, "spot output file opened");
                        Some(tokio::io::BufWriter::new(f))
                    }
                    Err(e) => {
                        tracing::error!(error = %e, path, "failed to open output file; spots will only be logged");
                        None
                    }
                }
            }
        };

    // Each per-window spawn sends its Vec<WsprSpot> result here so file I/O
    // stays serialised in this task.  Unbounded is safe: at most one entry per
    // SSRC per 2-minute window (typically < 30 concurrent decodes).
    let (spot_tx, mut spot_rx) = mpsc::unbounded_channel::<Result<Vec<WsprSpot>>>();

    loop {
        tokio::select! {
            biased;

            _ = shutdown.notified() => {
                tracing::info!("decode_task: shutdown signal received");
                break;
            }

            maybe_win = win_rx.recv() => {
                let (window, win_cfg) = match maybe_win {
                    Some(w) => w,
                    None    => { tracing::info!("window channel closed"); break; }
                };
                let tx = spot_tx.clone();
                // Spawn an independent task per window — all bands decode in
                // parallel, letting the next capture cycle start immediately.
                tokio::spawn(async move {
                    let result = decode_window(window, &win_cfg).await;
                    // Ignore send failure: decode_task may have exited on shutdown.
                    let _ = tx.send(result);
                });
            }

            maybe_result = spot_rx.recv() => {
                // recv() only returns None when every sender is dropped, which
                // cannot happen while `spot_tx` is still live in this scope.
                if let Some(result) = maybe_result {
                    match result {
                        Ok(spots) => {
                            if spots.is_empty() {
                                tracing::info!("wsprd: no spots decoded in this window");
                            }
                            for spot in &spots {
                                log_spot(spot);
                                write_spot(spot, &mut out_file).await;
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "wsprd decode failed");
                        }
                    }
                }
            }
        }
    }

    // Flush any buffered output before exiting.
    if let Some(ref mut w) = out_file {
        let _ = w.flush().await;
    }
}

/// Serialise a decoded spot to JSON and emit it via `tracing::info!`.
fn log_spot(spot: &WsprSpot) {
    match serde_json::to_string(spot) {
        Ok(json) => tracing::info!(spot = %json, "WSPR spot decoded"),
        Err(e) => tracing::error!(error = %e, "failed to serialise spot"),
    }
}

/// Append a single JSON line for `spot` to `out_file` (NDJSON format).
///
/// Errors are logged as warnings and do not abort the decode pipeline.
async fn write_spot(
    spot: &WsprSpot,
    out_file: &mut Option<tokio::io::BufWriter<tokio::fs::File>>,
) {
    let Some(ref mut w) = out_file else { return };
    match serde_json::to_string(spot) {
        Ok(json) => {
            // Write `<json>\n` then flush so each spot is durable immediately.
            let result = async {
                w.write_all(json.as_bytes()).await?;
                w.write_all(b"\n").await?;
                w.flush().await
            }
            .await;
            if let Err(e) = result {
                tracing::warn!(error = %e, "failed to write spot to output file");
            }
        }
        Err(e) => tracing::error!(error = %e, "failed to serialise spot for output file"),
    }
}
