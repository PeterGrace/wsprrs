# wsprrs

A WSPR decoder daemon for [ka9q-radio](https://github.com/ka9q/ka9q-radio), written in Rust.

`wsprrs` joins a ka9q-radio multicast group, listens to the status stream to discover active
mono USB-audio channels, buffers 2-minute PCM windows aligned to even UTC minutes, and runs
`wsprd` on each window.  Decoded spots are emitted as structured JSON via `tracing` and
optionally appended to an NDJSON file.  All bands decode in parallel — one independent
`wsprd` process per sealed window — so a 20-band setup decodes all bands simultaneously
rather than sequentially.

---

## Requirements

- A running `ka9q-radio` instance publishing audio over multicast UDP/RTP
- [`wsprd`](https://wsjt.sourceforge.io/wsjtx.html) (WSJT-X 2.5+) on your `PATH` or
  configured via `WSPR_WSPRD_PATH`
- Rust 2021 edition toolchain (`cargo build --release`)

---

## Building

```sh
cargo build --release
```

The binary is placed at `target/release/wsprrs`.

---

## Configuration

All settings are read from environment variables.  A `.env` file in the working directory is
loaded automatically if present.

| Variable               | Required | Default              | Description |
|------------------------|----------|----------------------|-------------|
| `WSPR_MULTICAST_ADDR`  | yes      | —                    | Multicast group IP (e.g. `239.1.2.3`) |
| `WSPR_MULTICAST_PORT`  | yes      | —                    | UDP port for the RTP audio stream (e.g. `5004`) |
| `WSPR_STATUS_PORT`     | no       | `5006`               | UDP port for the ka9q-radio status stream |
| `WSPR_LOCAL_ADDR`      | no       | `0.0.0.0`            | Local interface address for multicast joins |
| `WSPR_SSRC`            | no       | (all mono channels)  | Hex SSRC to track exclusively (e.g. `0x36FA`); omit to track all |
| `WSPR_CAPTURE_SECONDS` | no       | `116`                | Window length in seconds; must be ≥ 111 |
| `WSPR_TEMP_DIR`        | no       | `/tmp`               | Directory for per-decode temporary subdirectories |
| `WSPR_WSPRD_PATH`      | no       | `wsprd`              | Path or name of the `wsprd` binary |
| `WSPR_OUTPUT_FILE`     | no       | (none)               | Path to append decoded spots as NDJSON (one JSON object per line) |
| `WSPR_WISDOM_FILE`     | no       | `wspr_wisdom.dat`    | Path to the FFTW wisdom file (see [FFTW Wisdom](#fftw-wisdom)) |

### Example `.env`

```dotenv
WSPR_MULTICAST_ADDR=239.1.2.3
WSPR_MULTICAST_PORT=5004
WSPR_STATUS_PORT=5006
WSPR_LOCAL_ADDR=192.168.1.10
WSPR_WSPRD_PATH=/usr/local/bin/wsprd
WSPR_OUTPUT_FILE=/var/log/wspr/spots.ndjson
```

---

## Running

```sh
# With .env in the current directory:
./target/release/wsprrs

# Or with variables inline:
WSPR_MULTICAST_ADDR=239.1.2.3 WSPR_MULTICAST_PORT=5004 ./target/release/wsprrs
```

Log verbosity is controlled by the `RUST_LOG` environment variable:

```sh
RUST_LOG=info  ./target/release/wsprrs   # default — spot events + window lifecycle
RUST_LOG=debug ./target/release/wsprrs   # verbose — includes per-packet traces
RUST_LOG=warn  ./target/release/wsprrs   # quiet — errors and warnings only
```

Press `Ctrl-C` for a clean shutdown.

---

## How It Works

`wsprrs` runs four concurrent async tasks:

1. **recv_task** — reads raw UDP datagrams from the RTP audio port and forwards them to the
   buffer task via an in-process channel.

2. **status_task** — listens on the ka9q-radio status port; parses per-SSRC TLV metadata
   (centre frequency, sample rate, encoding, channel count).  Only mono USB-audio channels
   (`channels == 1`) are eligible for buffering — this is the standard WSPR output mode in
   ka9q-radio.

3. **buffer_task** — parses RTP headers, looks up each SSRC in the status map, ingests S16BE
   PCM samples into per-SSRC `AudioWindow` buffers aligned to even UTC minute boundaries, and
   seals each window when it reaches `WSPR_CAPTURE_SECONDS` seconds.  Centre frequency and
   sample rate come from the status stream; nothing is hard-coded.

4. **decode_task** — receives sealed windows.  Each window is dispatched to its own
   `tokio::spawn`ed task, so all bands decode concurrently.  Each task:
   - Creates an isolated temporary subdirectory under `WSPR_TEMP_DIR`
   - Copies `wspr_wisdom.dat` in (if available) to skip FFTW planning
   - Writes a standard RIFF/WAV file
   - Runs `wsprd -f <dial_MHz> wspr.wav` in that subdirectory
   - Reads `ALL_WSPR.TXT` for the full decode-quality field set
   - Copies `wspr_wisdom.dat` back out if this was the first run
   - Cleans up the temporary subdirectory

   Spot results flow back to the main decode loop via a channel for serialised file I/O.

---

## FFTW Wisdom

`wsprd` uses FFTW internally for its FFT computations.  After computing the optimal FFT plan
for the host CPU, it saves a `wspr_wisdom.dat` file in its working directory.  Subsequent
runs that find this file skip the planning step, which can save several seconds per decode.

`wsprrs` manages this automatically:

- **First run**: no wisdom file exists; `wsprd` computes and saves it; `wsprrs` copies it to
  `WSPR_WISDOM_FILE` for future use.
- **Subsequent runs**: `wsprrs` copies the wisdom file into each decode's temp subdirectory
  before `wsprd` starts.

The copy-out step uses an atomic rename, so concurrent decode tasks racing on the first run
are safe — FFTW wisdom is deterministic for a given CPU, so all tasks produce identical
content.

---

## Output

### Progress bars

While each 2-minute window is filling, a live progress bar is displayed per active channel:

```
SSRC 0x000036fa (14.0740 MHz) [===================>     ] 1044480/1392000 samples (75.0%) 75.0%
```

### Log events

```
INFO new audio window opened ssrc=14074 freq_hz=14074000 sample_rate=12000
INFO sealing audio window for decode ssrc=14074 samples_written=1391820 gap_count=0
INFO wsprd: no spots decoded in this window
INFO spot={"time_utc":"1106",...} WSPR spot decoded
```

### Decoded spots

Each spot is logged via `tracing::info!` and, if `WSPR_OUTPUT_FILE` is set, appended as a
single JSON line to that file.  Example spot (pretty-printed):

```json
{
  "time_utc": "1106",
  "snr_db": -14,
  "dt_sec": -0.23,
  "freq_hz": 3570071.2,
  "message": "W3POG FN20 23",
  "grid": "FN20",
  "power_dbm": 23,
  "drift": 0,
  "sync_quality": 0.76,
  "npass": 1,
  "osd_pass": 1,
  "nhardmin": 0,
  "decode_cycles": 50,
  "candidates": 37,
  "nfano": -85
}
```

### Spot field reference

| Field           | Type    | Source          | Description |
|-----------------|---------|-----------------|-------------|
| `time_utc`      | string  | derived         | UTC start of the WSPR window, `HHMM` |
| `snr_db`        | integer | ALL_WSPR.TXT    | Signal-to-noise ratio (dB re 2.5 kHz bandwidth) |
| `dt_sec`        | float   | ALL_WSPR.TXT    | Time offset from nominal window start (seconds) |
| `freq_hz`       | float   | ALL_WSPR.TXT    | Decoded carrier frequency (Hz) |
| `message`       | string  | derived         | Full WSPR message, e.g. `"K1ABC FN42 33"` |
| `grid`          | string  | ALL_WSPR.TXT    | Maidenhead locator, 4 or 6 characters; empty for type-2 messages |
| `power_dbm`     | integer | ALL_WSPR.TXT    | Transmitted power (dBm) |
| `drift`         | integer | ALL_WSPR.TXT    | Frequency drift (Hz/minute) |
| `sync_quality`  | float   | ALL_WSPR.TXT    | Sync vector quality (0–1); higher = cleaner lock |
| `npass`         | integer | ALL_WSPR.TXT    | Decode passes needed (1 = direct; 3 = required OSD) |
| `osd_pass`      | integer | ALL_WSPR.TXT    | OSD pass on which the decode succeeded |
| `nhardmin`      | integer | ALL_WSPR.TXT    | Minimum hard-decision count; more negative = more marginal |
| `decode_cycles` | integer | ALL_WSPR.TXT    | Decoder iterations used; higher = more effort required |
| `candidates`    | integer | ALL_WSPR.TXT    | Candidate messages explored; high values indicate a weak/noisy decode |
| `nfano`         | integer | ALL_WSPR.TXT    | Fano metric; large magnitude = strong clean decode |

**WSPR message types:**
- **Type 1** (standard): `<callsign> <grid4> <power>` — e.g. `K1ABC FN42 33`
- **Type 2** (no grid): `<callsign/prefix> <power>` — grid field is empty
- **Type 3** (hash): `<...> <grid6> <power>` — 6-character grid with compressed callsign hash

### Querying the NDJSON file

```sh
# All unique callsigns heard
jq -r '.message | split(" ")[0]' spots.ndjson | sort -u

# Spots sorted by SNR
jq -s 'sort_by(.snr_db) | reverse | .[]' spots.ndjson

# Count spots per band (by frequency bucket)
jq -r '.freq_hz / 1e6 | floor' spots.ndjson | sort | uniq -c
```

---

## Tokio Console

`wsprrs` is instrumented for [tokio-console](https://github.com/tokio-rs/console).  Start the
console in a separate terminal to inspect async task scheduling in real time:

```sh
tokio-console
```

---

## License

See `Cargo.toml` for author information.  No explicit license file is included at this time.
