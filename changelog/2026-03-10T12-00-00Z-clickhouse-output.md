# ClickHouse Output — 2026-03-10T12:00:00Z

## Summary

Adds optional ClickHouse insertion of decoded WSPR spots alongside the
existing NDJSON file output.  ClickHouse is enabled at runtime by setting
`WSPR_CLICKHOUSE_URL`; leaving it unset keeps behaviour identical to before.

## Changes

### `Cargo.toml`
- Added `clickhouse = "0.12"` dependency (HTTP RowBinary client).

### `src/spot.rs`
- Added `WsprSpotRow` struct: mirrors all `WsprSpot` fields and prepends
  `window_start_unix: i64` (Unix epoch seconds of the WSPR window boundary).
  Derives `clickhouse::Row`, `serde::Serialize`, and `serde::Deserialize`.
- Added `WsprSpotRow::from_spot(spot: &WsprSpot, window_start: SystemTime)`.
- `WsprSpot` itself is **unchanged** — `window_start_unix` does not appear in
  the NDJSON output.

### `src/config.rs`
- New fields on `Config`:
  - `clickhouse_url: Option<String>` — from `WSPR_CLICKHOUSE_URL`
  - `clickhouse_db: String` — from `WSPR_CLICKHOUSE_DB` (default `"default"`)
  - `clickhouse_table: String` — from `WSPR_CLICKHOUSE_TABLE` (default `"wspr_spots"`)
  - `clickhouse_user: Option<String>` — from `WSPR_CLICKHOUSE_USER`
  - `clickhouse_password: Option<String>` — from `WSPR_CLICKHOUSE_PASSWORD`
- Test `set_env` helper updated to clear all five new vars.

### `src/main.rs`
- Builds an optional `clickhouse::Client` from config before spawning tasks.
- `decode_task` accepts `Option<clickhouse::Client>`.
- Per-window spawn now returns `(Vec<WsprSpot>, SystemTime)` so `window_start`
  is available for `WsprSpotRow` construction without storing it on `WsprSpot`.
- Added `insert_spots_ch`: inserts an entire window's spots in a single
  ClickHouse HTTP round-trip; errors are logged and do not abort the pipeline.

## New environment variables

| Variable                  | Default       | Description                          |
|---------------------------|---------------|--------------------------------------|
| `WSPR_CLICKHOUSE_URL`     | *(disabled)*  | ClickHouse HTTP endpoint             |
| `WSPR_CLICKHOUSE_DB`      | `default`     | Database name                        |
| `WSPR_CLICKHOUSE_TABLE`   | `wspr_spots`  | Table name                           |
| `WSPR_CLICKHOUSE_USER`    | *(none)*      | Optional username                    |
| `WSPR_CLICKHOUSE_PASSWORD`| *(none)*      | Optional password — store in `.env`  |

## Suggested ClickHouse DDL

```sql
CREATE TABLE wspr_spots (
    window_start_unix Int64,
    time_utc          String,
    snr_db            Int32,
    dt_sec            Float32,
    freq_hz           Float64,
    message           String,
    callsign          String,
    grid              String,
    power_dbm         Int32,
    drift             Int32,
    sync_quality      Float32,
    npass             UInt8,
    osd_pass          UInt8,
    nhardmin          Int32,
    decode_cycles     UInt32,
    candidates        UInt32,
    nfano             Int32
) ENGINE = MergeTree()
ORDER BY (window_start_unix, callsign);
```

`toDateTime(window_start_unix)` converts the epoch column to a ClickHouse
`DateTime` for time-range queries and dashboards.
