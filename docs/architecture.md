# Architecture

hwstats is one Rust binary that collects statistics, keeps recent values in
memory, writes averaged history to SQLite and serves the web page and API.

```
                 ┌───────────── settings (watch channel) ─────────────┐
                 ▼                                                    │
  collectors ──► hub ──► SSE broadcast ──► browser (live)             │
  (one task      │  └──► per-series ring (15 min) ──► /api/live       │
   each)         │                                                    │
                 └──► averaged buckets ──► writer thread ──► SQLite ──┤
                                              │  rollups, pruning     │
  derived (power summary) ◄── hub latest ─────┘                       │
  backup manager ──► VACUUM INTO + gzip ──► backups/                  │
  HTTP (axum) ──► /api/history (SQLite tiers + ring), /api/settings ──┘
```

## Components

| Module | Role |
|---|---|
| `src/main.rs` | Command line (`serve`, `doctor`, `backup`, `restore`), start-up and shutdown order. |
| `src/config.rs` | Bootstrap configuration from `/etc/hwstats/config.toml`. |
| `src/settings.rs` | Runtime settings: defaults, validation and merge patches. |
| `src/collectors/` | One module per data source and the supervised scheduler (`mod.rs`). |
| `src/hub.rs` | Latest values, in-memory rings, persistence accumulators, SSE broadcast, collector status. |
| `src/derived.rs` | Series computed from other series (power summary). |
| `src/storage/` | SQLite schema, writer thread, rollups and pruning, history queries, corruption recovery. |
| `src/backup.rs` | Daily compressed backups, rotation and restore. |
| `src/health.rs` | systemd readiness, status text and watchdog. |
| `src/http/` | Routes, embedded assets, Host/Origin guard. |
| `src/procinfo.rs` | Details of one process, read from `/proc/<pid>` on request (`/api/process`). |
| `src/hostinfo.rs` | Static host facts for the page header. |
| `src/util/` | sysfs/procfs readers, counters, parsing, time, locking, rate-limited logging. |
| `web/` | The page: plain HTML, CSS and ES modules, no build step. Embedded into the binary at compile time. |

## Runtime layout

- A tokio runtime with two worker threads runs the collector loops, the
  HTTP server, the persistence ticker, the derived-series task, the backup
  scheduler and the heartbeat.
- Collector ticks run on the blocking pool (capped at 32 threads) so slow
  sysfs reads never block the runtime.
- The database writer is a dedicated thread that owns the only read-write
  SQLite connection. History queries open short-lived read-only connections.
- Backups run on their own low-priority thread (nice 19, idle I/O class).
- The watchdog runs on a plain thread and pings systemd only while the
  runtime heartbeat is fresh.

## Data flow

1. Each collector produces `(series descriptor, value)` pairs, text facts
   (`info`) and optional JSON (`extra`, used for processes).
2. The hub stores the latest value per series, appends to a 15-minute ring
   (at most 1800 points per series), adds the value to the persistence
   accumulator and broadcasts a compact JSON event to every SSE client.
3. Every `history.resolution_s` seconds the accumulators are drained and the
   averages are queued for the writer thread.
4. The writer inserts rows, rolls raw data up to 1-minute and 1-hour tiers
   and applies retention.
5. The page reads `/api/meta` (catalog), `/api/snapshot` (latest values) and
   `/api/live` (ring backfill), then follows `/api/stream`. History ranges use
   `/api/history`.

## Files on disk

| Path | Content |
|---|---|
| `/usr/local/bin/hwstats` | Binary (page assets included). |
| `/usr/local/bin/hwstats.previous` | Binary from before the last upgrade (for rollback). |
| `/etc/hwstats/config.toml` | Bootstrap configuration (never overwritten by upgrades). |
| `/etc/hwstats/config.toml.default` | The shipped default configuration, for comparison after upgrades. |
| `/etc/systemd/system/hwstats.service` | Service unit. |
| `/var/lib/hwstats/hwstats.db` | SQLite database (WAL mode; `-wal`/`-shm` side files). |
| `/var/lib/hwstats/backups/` | `hwstats-<UTC stamp>.db.gz` backups. |
| `/var/lib/hwstats/hwstats.lock` | Single-instance lock. |
| `/var/lib/hwstats/nvml-init.marker` | Present only while NVML is initialising. |
| `/usr/local/share/doc/hwstats/` | README and docs. |
