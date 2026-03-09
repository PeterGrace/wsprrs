# Deduplicate unsupported RTP payload type warnings

**Date:** 2026-03-09T01:53:44Z

## Problem

When ka9q-radio multicasts non-IQ streams (e.g. PT 122) on the same multicast
group as the IQ stream, `wsprrs` emitted a `WARN` log line for every single
incoming packet, flooding the log at typical packet rates.

## Change

`src/main.rs` — `buffer_task`:

- Added `warned_pt: HashSet<u8>` to track payload types that have already
  triggered a warning.
- On the first packet with an `UnsupportedPayloadType` error, emit a single
  `WARN` noting the PT and that further packets will be silently dropped.
- Subsequent packets with the same PT are logged at `TRACE` level only.
- All other `WsprError` variants continue to emit a `WARN` per packet (they
  indicate genuine data corruption and should not be silenced).

## Files changed

- `src/main.rs`
