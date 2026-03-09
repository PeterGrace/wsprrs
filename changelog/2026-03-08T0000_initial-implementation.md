# wsprrs — Initial Implementation

**Date:** 2026-03-08
**Author:** Claude Sonnet 4.6

---

## Summary

Complete proof-of-concept implementation of a ka9q-radio WSPR decoder.  Receives IQ
data from a running `ka9q-radio` instance over multicast UDP/RTP, buffers it into
2-minute windows aligned to even UTC minutes, decodes spots by calling `wsprd` as a
subprocess, and logs decoded spots to stdout as JSON.

---

## Files Added / Modified

| Path | Status | Notes |
|------|--------|-------|
| `Cargo.toml` | modified | Added `anyhow`, `bytes`, `dotenvy`, `indicatif`, `regex`, `serde_json`, `socket2`, `tempfile`, `zerocopy`; expanded `tokio` features; removed `lazy_static` and `dotenv` |
| `src/main.rs` | modified | Rewritten: async `#[tokio::main]`, task orchestration, graceful Ctrl-C shutdown |
| `src/error.rs` | new | `WsprError` via `thiserror` |
| `src/config.rs` | new | `Config::from_env()` with validation |
| `src/rtp.rs` | new | RTP header + TLV metadata parsing, zero-copy payload extraction |
| `src/buffer.rs` | new | `IqWindow` pre-allocated sample buffer, even-minute alignment |
| `src/multicast.rs` | new | `socket2` multicast join, async receive loop |
| `src/decode.rs` | new | `.c2` file writer, `wsprd` subprocess, `OnceLock<Regex>` output parser |
| `src/spot.rs` | new | `WsprSpot` data type with `serde::Serialize` |

---

## Architecture

```
[UdpSocket]
    │ recv_from loop (single reused buffer, try_send on full)
    ▼
[mpsc::channel<ReceivedPacket>  cap=1024]
    ▼
[buffer_task]
    │ tokio::time::interval(1 s) seals expired windows
    │ HashMap<u32, IqWindow> per SSRC
    │ indicatif::ProgressBar per active window
    ▼
[mpsc::channel<IqWindow>  cap=8]
    ▼
[decode_task]
    │ spawn_blocking: i16→f32 + write .c2 tempfile
    │ tokio::process::Command for wsprd (async)
    │ OnceLock<Regex> for output parsing
    │ tracing::info! each spot as JSON
```

Shutdown via `tokio::sync::Notify` triggered by `ctrlc` handler; all three tasks
`select!` on it.

---

## Key Design Decisions

- **i16 storage, f32 on write:** IQ samples stored as `i16` (half the memory of `f32`);
  converted to `f32` only when writing the `.c2` file, deferred to a `spawn_blocking`
  thread so the Tokio reactor is never stalled.
- **Zero-copy RTP parsing:** No byte copies during header/TLV parsing; `iq_payload` is
  a borrowed `&[u8]` slice into the original receive buffer.
- **Gap-resilient buffering:** Pre-zeroed `Vec<i16>` means dropped RTP packets produce
  silence — the correct fill value for `wsprd`.
- **OnceLock regex:** The `wsprd` output regex is compiled exactly once and reused across
  all decode calls with no locking overhead.
- **tempfile auto-cleanup:** `NamedTempFile` is held alive until `wsprd` exits, then
  dropped — the OS deletes the file even on panic.

---

## Tests (23 passing)

| Module | Tests |
|--------|-------|
| `buffer` | boundary alignment, capacity, ingest offset, gap detection, overflow error |
| `config` | required vars, hex SSRC, short capture rejection |
| `decode` | typical spot, negative drift, garbage line, multi-line, `.c2` file size, UTC decomp |
| `rtp` | PT_IQ8, PT_IQ+TLV, too-short, bad version, bad PT, malformed TLV, CSRC skip |

---

## External Prerequisite

`wsprd` must be installed:
```bash
dnf install wsjtx          # Fedora
apt install wsjtx           # Debian/Ubuntu
```

---

## Usage

```bash
# Minimum required variables
WSPR_MULTICAST_ADDR=239.x.x.x \
WSPR_MULTICAST_PORT=5004 \
WSPR_CENTER_FREQ_HZ=14097000 \
RUST_LOG=info cargo run

# Expected output when a WSPR window closes with spots:
# INFO spot={"time_utc":"2300","snr_db":-18,...} WSPR spot decoded
```

---

## Quality Checklist

- [x] `cargo test` — 23/23 pass
- [x] `cargo build` — zero warnings
- [x] `cargo clippy -- -D warnings` — clean
- [x] `cargo fmt --check` — clean
- [x] All public items have doc comments
- [x] No commented-out code or debug statements
- [x] No hardcoded credentials
