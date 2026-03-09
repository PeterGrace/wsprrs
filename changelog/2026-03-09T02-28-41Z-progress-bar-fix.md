# Progress Bar Fix — 2026-03-09T02:28:41Z

## Problem

Progress bars from `indicatif::MultiProgress` were never visible because:

1. **`tracing_subscriber` and `indicatif` both write to stderr directly.**
   When `tracing` wrote a log line, the raw write to the stderr FD would land
   _between_ indicatif's cursor-movement ANSI escape sequences, corrupting the
   progress bar rendering.  Under heavy logging, the bars effectively never
   appeared.

2. **No `info`-level feedback** while waiting for the status stream to declare a
   stereo IQ channel.  With `RUST_LOG=info` the user saw only four startup lines
   and then silence — even when data was flowing — because:
   - Unknown-SSRC data packets were logged at `trace` level.
   - Status stream channel registration was logged at `debug` level.

## Changes

### `src/main.rs`

- Added `MultiProgressMakeWriter` / `MultiProgressEventWriter` — a custom
  `tracing_subscriber::fmt::MakeWriter` implementation that buffers each tracing
  event and emits it via `MultiProgress::println` on `Drop`.  This ensures every
  log line is printed above the progress bars without corrupting them.
- `MultiProgress` is now created in `main()` and stored in `Arc<MultiProgress>`,
  shared between the tracing writer and `buffer_task`.
- `buffer_task` now accepts `multi: Arc<MultiProgress>` instead of constructing
  its own `MultiProgress::new()` internally.
- Unknown-SSRC trace log raised to `debug` (visible with `RUST_LOG=debug`).

### `src/status.rs`

- `"channel info updated"` log raised from `debug` to `info`, now includes
  `encoding` field.  Renamed to `"IQ channel registered"` for clarity.
  Users will see this line at `RUST_LOG=info` as soon as the status stream
  delivers a usable stereo IQ channel.

## Behaviour After Fix

```
2026-03-09T02:20:29Z  INFO wsprrs starting
2026-03-09T02:20:29Z  INFO joined multicast group 239.107.139.201:5004 (data)
2026-03-09T02:20:29Z  INFO joined multicast group 239.107.139.201:5006 (status)
2026-03-09T02:20:30Z  INFO IQ channel registered ssrc=0x00D5AD28 freq_hz=14097000 ...
SSRC 0x00D5AD28 (14.097 MHz) [=========================>  ] 987136/1392000 ...
```

Progress bar appears as soon as the first IQ packet arrives for a registered
stereo channel and updates on each ingested packet.
