# RTP-Relative Buffer Offset — 2026-03-09T03:05:54Z

## Problem

`AudioWindow::ingest()` computed the sample write offset using:

```
offset = rtp_timestamp − (window_start_unix_secs × sample_rate_hz) mod 2^32
```

This assumes ka9q-radio's RTP clock runs from Unix epoch × sample_rate as its
origin.  In practice ka9q-radio chooses an arbitrary epoch (typically based on
process start time), so the computed offset was wrong — often wildly negative
(wrapped to a huge positive), causing an immediate `BufferOverflow` on every
single channel every time a window opened.  The windows were dropped, recreated
on the next packet, and overflowed again indefinitely.

## Fix

Replace the Unix-epoch-based formula with a two-phase relative scheme:

### Phase 1 — First packet in a window

The write position is derived purely from **wall-clock elapsed time** since the
window boundary:

```
start_offset = (SystemTime::now() − window_start).as_secs_f64() × sample_rate_hz
```

This is anchored to real time and requires no assumptions about the RTP clock
origin.  The value is clamped to `[0, capacity_samples − 1]`.

### Phase 2 — All subsequent packets

```
offset = start_offset + (rtp_timestamp − rtp_base)   [with 32-bit wrap correction]
```

`rtp_base` is the RTP timestamp of the first packet.  All subsequent offsets are
*relative*, so any arbitrary epoch cancels out.

## New Fields on `AudioWindow`

| Field | Type | Purpose |
|---|---|---|
| `rtp_base` | `Option<u32>` | First packet's RTP timestamp; `None` until first ingest |
| `start_offset` | `usize` | Buffer index where the first packet was placed |

## Wrap-Around Handling

A `delta < -(RTP_WRAP / 2)` is treated as a forward wrap (u32 rollover), which
is the only plausible interpretation within a 2-minute WSPR window at ≤ 192 kHz.
Genuine backwards packets (reorders/duplicates) with a small negative delta are
silently discarded.

## Test Changes

All three `ingest_*` tests previously used `window_start = UNIX_EPOCH`, which
caused the new wall-clock elapsed path to compute an enormous `start_offset`.
Tests now use `window_start = SystemTime::now()` so elapsed ≈ 0 and
`start_offset = 0`, preserving the original test semantics while working
correctly with the new implementation.

`ingest_overflow_returns_error` was also updated: it now sends a first packet to
establish the RTP base, then a second packet with a 200-second RTP delta that
exceeds `capacity_samples`, correctly triggering the overflow.
