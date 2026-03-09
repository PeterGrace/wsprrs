# ka9q-radio Protocol Redesign

**Date:** 2026-03-09T02:00:00Z
**Status:** Complete
**Tests:** 38 passing (0 failing)

---

## Root Cause: Five Incorrect Protocol Assumptions

The initial implementation (2026-03-08) and the subsequent minor patch (2026-03-09T01-53-44Z) both rested on fundamentally wrong assumptions about the ka9q-radio protocol. These are documented here in detail to prevent recurrence.

### 1. PT 96/97/98 Are NOT Reserved IQ Types

**What we assumed:** Payload types 96, 97, and 98 were hard-coded as the only valid IQ types. RTP packets with any other PT were rejected with `UnsupportedPayloadType` errors.

**Reality:** ka9q-radio allocates RTP payload types dynamically. PT numbers have no fixed meaning across installations. The actual PT in use for any given channel is reported only via the status stream (port 5006, TLV tag 105 = `RTP_PT`). Hard-coding PT 96/97/98 guaranteed that real ka9q-radio traffic would be silently dropped.

**Fix:** Removed the PT whitelist entirely from `parse_rtp_packet`. All PTs are now accepted at the RTP parsing layer. Filtering is performed in `buffer_task` based on per-SSRC `ChannelInfo.channels == 2` (stereo IQ), not on the PT number.

### 2. TLV Metadata Is NOT Embedded in RTP Data Packets

**What we assumed:** PT 97 ("PT_IQ") packets contained a TLV metadata block immediately after the RTP fixed header, before the sample payload. The `parse_tlv_metadata` function and `RtpMetadata` struct were built to extract `center_freq_hz` and `sample_rate_hz` from this block.

**Reality:** The TLV chain is sent as a completely separate UDP datagram on port 5006 (the status port), on the same multicast group IP as the data stream. There is no metadata embedded in data packets at all. The `parse_tlv_metadata` function was parsing garbage from the beginning of the IQ sample payload.

**Fix:** Deleted `parse_tlv_metadata`, `RtpMetadata`, and the `MalformedTlv` error variant. Created a new `src/status.rs` module that correctly parses the ka9q-radio status packet format. Added a `status_task` that binds a separate socket on port 5006 and populates a shared `HashMap<u32, ChannelInfo>` keyed by SSRC.

### 3. S16BE Not S16LE

**What we assumed:** ka9q-radio IQ samples were 16-bit signed little-endian (S16LE). The `ingest` function decoded every sample pair with `i16::from_le_bytes`.

**Reality:** ka9q-radio IQ output uses 16-bit signed big-endian (S16BE). The encoding enum in `rtp.h` defines `S16LE=1, S16BE=2`, and the status stream reports `OUTPUT_ENCODING = 107` with value 2 for standard IQ streams. Every decoded sample was byte-swapped, producing completely wrong IQ data and guaranteeing zero valid WSPR spots.

**Fix:** `IqWindow::ingest` now accepts an `encoding: u8` parameter. A `decode_sample: fn([u8; 2]) -> i16` function pointer is selected once before the decode loop based on whether `encoding == ENC_S16BE` or not. The selection is outside the loop so the branch does not repeat per-sample — the compiler auto-vectorizes the inner loop for either variant.

### 4. Center Frequency Must Not Be a Required Environment Variable

**What we assumed:** `WSPR_CENTER_FREQ_HZ` was a required env var. Without it, the program refused to start.

**Reality:** Center frequency is a per-channel property reported by the status stream via TLV tag 33 (`RADIO_FREQUENCY`). Requiring it as an env var prevented multi-band operation and forced the user to know the SDR tuning before starting. The status stream is the authoritative source.

**Fix:** Removed `center_freq_hz` and `sample_rate_hz` from `Config`. Added `status_port: u16` (default 5006, env var `WSPR_STATUS_PORT`). `IqWindow` now carries `center_freq_hz: f64` set at construction time from the status-derived `ChannelInfo`. The decode task reads `window.center_freq_hz` directly.

### 5. PT 122 Was Never Wrong — Our Interpretation Was Wrong

**What we assumed:** PT 122 was an unsupported payload type to be rejected. The `warned_pt` deduplication logic (added in the 2026-03-09T01-53-44Z patch) worked around the symptom but did not fix the root cause.

**Reality:** PT 122 is 12 kHz mono S16BE PCM — demodulated audio output from ka9q-radio. It is a completely valid packet. The correct reason to skip it is `channels == 1` (mono, not IQ), not because the PT number is wrong. With the new architecture, PT 122 packets are accepted by the RTP parser, looked up in the channel map (where `channels = 1`), and silently skipped by `buffer_task` because `info.is_iq_ready()` returns `false` for mono channels.

---

## Correct ka9q-radio Protocol Description

### Ports (same multicast group for both)

| Port | Content |
|------|---------|
| 5004 | RTP data stream (IQ samples, audio, etc.) |
| 5006 | Status/control TLV packets |

### Status Packet Format

```
byte 0:    pkt_type (0 = STATUS, 1 = CMD)
bytes 1+:  TLV chain
           [tag: u8] [length: variable] [value: BE, leading zeros suppressed]
           tag 0 = EOL sentinel (no length or value follows)
```

### TLV Length Encoding

- `first_byte < 0x80`: literal byte count (short form)
- `first_byte >= 0x80`: low 7 bits = N (1-4), followed by N big-endian bytes holding the actual length

### Value Encoding

All values are big-endian with leading zero bytes suppressed. Zero is encoded as zero-length (no value bytes). `f64` values are stored as their `u64` bit-pattern encoded the same way; decode with `f64::from_bits`.

### Key TLV Tags (from `ka9q-radio include/status.h`)

| Tag | Name | Type | Meaning |
|-----|------|------|---------|
| 18 | `OUTPUT_SSRC` | u32 | Which data-stream SSRC this status describes |
| 20 | `OUTPUT_SAMPRATE` | u32 | Sample rate in Hz |
| 33 | `RADIO_FREQUENCY` | f64 | Centre frequency in Hz |
| 49 | `OUTPUT_CHANNELS` | u32 | 1=mono audio, 2=stereo IQ |
| 105 | `RTP_PT` | u8 | PT used on the companion data stream |
| 107 | `OUTPUT_ENCODING` | u8 | 1=S16LE, 2=S16BE, 3=OPUS, 4=F32LE, ... |

### Encoding Enum (from `ka9q-radio rtp.h`)

```
NO_ENCODING=0, S16LE=1, S16BE=2, OPUS=3, F32LE=4
```

### Common Channel Types

| PT (typical) | Rate | Channels | Encoding | Description |
|-------------|------|----------|----------|-------------|
| 122 | 12000 | 1 | S16BE | Demodulated mono audio |
| 123 | 12000 | 2 | S16BE | Stereo IQ (WSPR target) |

PT assignments are dynamic and may differ across installations. Always use the status stream.

---

## Files Changed

### NEW: `src/status.rs`

Complete new module implementing the ka9q-radio status stream.

- `ENC_S16LE`, `ENC_S16BE` — public encoding constants used by `buffer.rs`
- `ChannelInfo` — per-SSRC metadata struct with `is_iq_ready()` predicate
- `process_status_packet(buf, map)` — pure function: parses one status datagram, applies updates to the channel map
- `receive_loop(socket, channel_map, shutdown)` — async task loop
- Private helpers: `read_tlv_len`, `decode_u32`, `decode_f64`
- 14 unit tests covering CMD packet rejection, EOL-only packets, partial packets, full channel info parsing, leading-zero suppression, unknown tag skipping, `is_iq_ready` semantics, and TLV length decoding edge cases

### REWRITTEN: `src/rtp.rs`

Drastically simplified.

- Removed: `PT_IQ`, `PT_IQ8`, `PT_IQ12` constants; `RtpMetadata` struct; `parse_tlv_metadata` function; PT whitelist check
- Removed: `bad_payload_type_returns_error`, `parse_pt_iq_with_tlv`, `malformed_tlv_overrun_returns_error` tests (no longer valid)
- Added: `parse_returns_full_payload_for_any_pt` test (PT 122 parses without error)
- Added: `extension_block_is_skipped` test
- Renamed: `parse_pt_iq8_no_payload` → `parse_any_pt_no_payload`

### MODIFIED: `src/error.rs`

- Removed: `UnsupportedPayloadType` variant (no longer returned)
- Removed: `MalformedTlv` variant (TLV is not in data packets)

### REWRITTEN: `src/config.rs`

- Removed: `center_freq_hz: f64`, `sample_rate_hz: u32` fields and their env vars
- Added: `status_port: u16` field (default 5006, env var `WSPR_STATUS_PORT`)
- Updated doc comment to reflect that freq/rate come from the status stream
- Tests: replaced ad-hoc `set_var/remove_var` calls with a `static ENV_LOCK: Mutex<()>` to serialize parallel test threads; added `from_env_parses_custom_status_port` test

### MODIFIED: `src/buffer.rs`

- `IqWindow`: added `center_freq_hz: f64` field
- `IqWindow::new`: added `center_freq_hz: f64` as second parameter
- `IqWindow::ingest`: added `encoding: u8` parameter; byte-order dispatch via `fn([u8; 2]) -> i16` function pointer selected before the loop
- Tests: updated to use `to_be_bytes()` and pass `ENC_S16BE`; updated `IqWindow::new` calls to include `14_097_000.0` freq parameter

### MODIFIED: `src/multicast.rs`

- `build_socket(cfg, port)`: added `port: u16` parameter (was hardcoded to `cfg.multicast_port`); may now be called with either `cfg.multicast_port` or `cfg.status_port`

### REWRITTEN: `src/main.rs`

- Added `mod status`, `use status::ChannelInfo`, `use tokio::sync::RwLock`
- Removed `HashSet` import and `warned_pt` logic entirely
- Created `channel_map: Arc<RwLock<HashMap<u32, ChannelInfo>>>`
- Builds two sockets: data socket (port `cfg.multicast_port`) and status socket (port `cfg.status_port`)
- Spawns 4 tasks: `recv`, `status` (new), `buffer`, `decode`
- `buffer_task`: reads `channel_map` under an `RwLock` read lock per packet; skips SSRCs that are unknown or not `is_iq_ready()`; creates `IqWindow` with `info_freq` and `info_rate` from the map; calls `ingest` with `info_encoding`
- `decode_task`: reads `window.center_freq_hz` instead of `cfg.center_freq_hz`

### MODIFIED: `src/decode.rs`

- Updated `c2_file_is_written` test to use the new 4-argument `IqWindow::new` signature

---

## Architecture: Before and After

### Before (3 tasks)

```
recv_task → [pkt_channel] → buffer_task → [win_channel] → decode_task
                                 |
                          cfg.center_freq_hz (static)
                          cfg.sample_rate_hz (static)
                          PT whitelist: {96, 97, 98}
                          TLV in data packets (wrong)
                          i16 LE decode (wrong byte order)
```

### After (4 tasks)

```
recv_task   → [pkt_channel] → buffer_task → [win_channel] → decode_task
                                   |
status_task → [channel_map RwLock] ┘
              per-SSRC: center_freq, sample_rate, channels, encoding, pt
              filter: channels == 2 (stereo IQ only)
              byte order: i16 BE (S16BE from status stream)
```

---

## Lessons Learned

1. **Always verify protocol assumptions against the actual source code before implementation.** The ka9q-radio source (`include/status.h`, `rtp.h`) precisely defines the TLV tag numbers, encoding enum values, and packet format. Reading it first would have prevented all five bugs.

2. **Dynamic protocol metadata belongs in the protocol, not the config file.** Requiring `WSPR_CENTER_FREQ_HZ` as an env var was a smell: it duplicated information that the protocol already carries, and it broke multi-band operation by design.

3. **Byte order bugs are silent.** The S16LE vs S16BE error produced plausible-looking (but completely wrong) IQ data. No assertion would catch it at the RTP parsing layer; only end-to-end testing against a real ka9q-radio instance or careful protocol review would reveal it.

4. **Test isolation for process-global state requires explicit synchronisation.** Env var tests that run in parallel threads need a mutex (or `--test-threads=1`) to be deterministic. The `static Mutex<()>` pattern is the idiomatic Rust solution without adding external crates.
