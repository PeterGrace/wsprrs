# wsprrs

A WSPR decoder daemon for [ka9q-radio](https://github.com/ka9q/ka9q-radio), written in Rust.

`wsprrs` joins a ka9q-radio multicast group, buffers 2-minute IQ windows aligned to even UTC
minutes, calls `wsprd` to decode WSPR spots, and emits each spot as a JSON object on stdout.

---

## Requirements

- A running `ka9q-radio` instance publishing an IQ stream over multicast UDP/RTP
- [`wsprd`](https://physics.princeton.edu/pulsar/K1JT/wsjtx.html) on your `PATH` (or configured
  via `WSPR_WSPRD_PATH`)
- Rust 2021 edition toolchain (`cargo build --release`)

---

## Building

```sh
cargo build --release
```

The binary is placed at `target/release/wsprrs`.

---

## Configuration

All settings are read from environment variables. A `.env` file in the working directory is loaded
automatically if present.

| Variable               | Required | Default   | Description                                                    |
|------------------------|----------|-----------|----------------------------------------------------------------|
| `WSPR_MULTICAST_ADDR`  | yes      | —         | Multicast group IP address (e.g. `239.1.2.3`)                  |
| `WSPR_MULTICAST_PORT`  | yes      | —         | UDP port for the RTP data stream (e.g. `5004`)                 |
| `WSPR_STATUS_PORT`     | no       | `5006`    | UDP port for the ka9q-radio status stream                      |
| `WSPR_LOCAL_ADDR`      | no       | `0.0.0.0` | Local interface address to bind and join the multicast group   |
| `WSPR_SSRC`            | no       | (all IQ)  | Hex SSRC to track exclusively (e.g. `0x0AE2B400`); omit to track all stereo-IQ SSRCs |
| `WSPR_CAPTURE_SECONDS` | no       | `116`     | Window length in seconds; must be >= 111                       |
| `WSPR_TEMP_DIR`        | no       | `/tmp`    | Directory for temporary `.c2` files passed to `wsprd`          |
| `WSPR_WSPRD_PATH`      | no       | `wsprd`   | Path or name of the `wsprd` binary                             |

### Example `.env`

```dotenv
WSPR_MULTICAST_ADDR=239.1.2.3
WSPR_MULTICAST_PORT=5004
WSPR_STATUS_PORT=5006
WSPR_LOCAL_ADDR=192.168.1.10
WSPR_WSPRD_PATH=/usr/local/bin/wsprd
```

---

## Running

```sh
# With .env in the current directory:
./target/release/wsprrs

# Or with variables inline:
WSPR_MULTICAST_ADDR=239.1.2.3 WSPR_MULTICAST_PORT=5004 ./target/release/wsprrs
```

Log verbosity is controlled by the standard `RUST_LOG` environment variable:

```sh
RUST_LOG=debug ./target/release/wsprrs   # verbose
RUST_LOG=warn  ./target/release/wsprrs   # quiet
```

Press `Ctrl-C` to trigger a clean shutdown.

---

## How It Works

`wsprrs` runs four concurrent async tasks:

1. **recv_task** — reads raw UDP datagrams from the RTP data port and passes them to the buffer task.
2. **status_task** — listens on the status port; parses per-SSRC TLV metadata (centre frequency,
   sample rate, encoding, channel count) emitted by ka9q-radio.
3. **buffer_task** — parses RTP headers, filters for stereo IQ streams (channels == 2), ingests
   S16BE samples into per-SSRC `IqWindow` ring buffers aligned to even UTC minute boundaries, and
   seals each window after `WSPR_CAPTURE_SECONDS` seconds.
4. **decode_task** — receives sealed windows, writes a `.c2` file to `WSPR_TEMP_DIR`, invokes
   `wsprd` as a subprocess, parses its output, and logs each decoded spot as JSON.

Centre frequency and sample rate are never hard-coded; they are discovered per-SSRC from the
ka9q-radio status stream.

---

## Expected Output

### Startup

```
2026-03-09T02:01:00.123456Z  INFO wsprrs starting src/main.rs:84
2026-03-09T02:01:00.124Z     INFO configuration loaded multicast=239.1.2.3 data_port=5004 status_port=5006 ssrc_filter=None
2026-03-09T02:01:00.125Z     INFO joined multicast group 239.1.2.3:5004 (data)
2026-03-09T02:01:00.125Z     INFO joined multicast group 239.1.2.3:5006 (status)
```

### While Buffering

A progress bar per active IQ stream is displayed on stderr while each 2-minute window fills:

```
SSRC 0x0ae2b400 (14.097 MHz) [===================>     ] 221184/288000 samples (76.8%) 76.8%
```

The SSRC prefix encodes the centre frequency in MHz, matching the ka9q-radio convention where
`ssrc * 1e-6 = MHz`.

Log lines accompany each window lifecycle event:

```
INFO new IQ window opened ssrc=183058432 freq_hz=14097000 sample_rate=12000
INFO sealing IQ window for decode ssrc=183058432 samples_written=288000 gap_count=0
```

### Decoded Spots

Each decoded spot is emitted as a single JSON object on a `tracing::info!` line:

```json
{
  "time_utc": "0200",
  "snr_db": -14,
  "dt_sec": 0.4,
  "freq_hz": 14097042.7,
  "message": "K1ABC FN42 33",
  "grid": "FN42",
  "power_dbm": 33,
  "drift": 0,
  "decode_cycles": 3,
  "jitter": 2
}
```

The raw log line looks like:

```
2026-03-09T02:02:02.001Z  INFO spot={"time_utc":"0200","snr_db":-14,...} WSPR spot decoded
```

When no spots are decoded in a window:

```
INFO wsprd: no spots decoded in this window
```

### Shutdown

```
INFO Ctrl-C received; shutting down
INFO buffer_task: shutdown signal received
INFO decode_task: shutdown signal received
INFO wsprrs stopped
```

---

## Spot JSON Field Reference

| Field           | Type    | Description                                              |
|-----------------|---------|----------------------------------------------------------|
| `time_utc`      | string  | UTC time of transmission as `HHMM`                       |
| `snr_db`        | integer | Signal-to-noise ratio (dB re 2.5 kHz bandwidth)          |
| `dt_sec`        | float   | Time offset from nominal window start (seconds)          |
| `freq_hz`       | float   | Decoded carrier frequency (Hz)                           |
| `message`       | string  | Full WSPR message, e.g. `"K1ABC FN42 33"`                |
| `grid`          | string  | Maidenhead grid locator extracted from the message       |
| `power_dbm`     | integer | Transmitted power (dBm)                                  |
| `drift`         | integer | Frequency drift (Hz/minute)                              |
| `decode_cycles` | integer | Number of decoder correlation cycles used                |
| `jitter`        | integer | Decoder jitter metric                                    |

---

## Logging Spots to a File

Because spots are written via `tracing::info!`, you can redirect or tee them independently:

```sh
# Append all spot JSON lines to a file while still watching the terminal
./target/release/wsprrs 2>&1 | tee -a spots.log

# Extract only spot JSON with jq
./target/release/wsprrs 2>&1 | grep 'WSPR spot decoded' | sed 's/.*spot=\(.*\) WSPR.*/\1/' | jq .
```

---

## Tokio Console

`wsprrs` is instrumented for [tokio-console](https://github.com/tokio-rs/console). Start the
console in a separate terminal to inspect async task scheduling in real time:

```sh
tokio-console
```

---

## License

See `Cargo.toml` for author information. No explicit license file is included at this time.
