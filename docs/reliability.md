# Reliability

The service is built to keep running whatever the hardware or the disk does.

## Collector supervision

Every collector has its own loop (`src/collectors/mod.rs`). Each tick runs
on the blocking thread pool inside `catch_unwind` and a timeout of
2× the interval, between 2 and 10 seconds.

- **Panic.** The collector instance is dropped and rebuilt from its factory
  after a backoff (5 s doubling up to 5 min). Other collectors are not
  affected. A panic hook logs the panic with a backtrace, rate-limited per
  location.
- **Error result.** The collector state becomes `error` and it retries with
  a backoff (2× the interval, doubling, up to 60 s).
- **Timeout.** The state becomes `stalled`. The stuck tick keeps its thread,
  and no new tick starts until it returns, so blocked threads cannot pile
  up. Its results are discarded when it finally returns late.
- **No source.** A collector reporting `unavailable` is re-probed every 30 s
  only.
- **Hardware rescan.** The device-discovering collectors drop their
  instance and tick at once, which also ends a re-probe wait or an error
  backoff. One rescan at a time, at least 10 s apart. See
  [hardware.md](hardware.md#rescan).
- **Bounded threads.** The blocking pool is capped at 32 threads.

## NVML crash guard

NVML is a C library. Before initialising it, the collector writes
`nvml-init.marker` (containing the boot id) into the state directory and
removes it afterwards. If the process dies inside NVML initialisation, the
next start finds the marker with the same boot id. NVML is then skipped for
the rest of that boot and `nvidia-smi` is used. The reason appears in the
status.

## Database off the critical path

- The live page never depends on SQLite.
- The writer queue is bounded and drops the oldest batches when full.
- Write errors cause backoff, never exit.
- A database that cannot be opened switches the service to memory-only
  mode. A corrupt database is quarantined and restored from the newest valid
  backup. See [storage.md](storage.md#recovery).

## Bounded resources

| Resource | Limit |
|---|---|
| Live series | 2000; new series beyond it are ignored and counted |
| Ring per series | 15 minutes or 1800 points |
| SSE clients | 32; slow clients get a `resync` event instead of growing buffers |
| Process detail reads (`/api/process`) | 4 at a time on the blocking pool, 3 s timeout; a read stuck in the kernel keeps its slot until it returns, so stuck reads cannot pile up |
| systemd unit | `MemoryMax=512M`, `TasksMax=128`, `Nice=5` |

Series not updated for 10 minutes leave the live catalog; their history
stays in the database.

## Network edge

- The HTTP server binds to `127.0.0.1:9393` by default. If binding fails
  (port in use), it retries every 10 s while collection and history continue.
- A Host/Origin guard rejects requests whose Host header is not a local
  name (`localhost`, `127.0.0.1`, `::1`, the listen address or a name in
  `allowed_hosts`). Writes with a foreign `Origin` are refused too. This
  blocks DNS-rebinding pages from changing settings.

## Process and systemd

- **Single instance.** `flock` on `<state dir>/hwstats.lock`. A second
  instance using the same database exits with a clear message.
- **Readiness.** `Type=notify`: `READY=1` is sent after start-up and the
  status line shows the URL.
- **Watchdog.** `WatchdogSec=60`. A watchdog thread pings systemd only while
  an async heartbeat task keeps updating, so a wedged runtime gets restarted.
- **Restarts.** `Restart=always`, `RestartSec=5`, `StartLimitIntervalSec=0`:
  systemd never gives up.
- **Shutdown.** SIGTERM stops the collectors and the HTTP server gracefully,
  flushes pending values, checkpoints the WAL and exits within
  `TimeoutStopSec`.

## Bad input

- A missing or invalid `config.toml` produces a warning and defaults.
- Stored settings that do not parse are reset to defaults, with a note in
  the page.
- Settings updates are merged and clamped to safe ranges. Wrong types are
  rejected with HTTP 400.
- Parsers ignore unknown or missing fields. Processes that exit while being
  read are skipped.

## Logging

Logs go to stderr, which means the journal under systemd:
`journalctl -u hwstats`. Repeated errors with the same key are logged once,
then at most every 10 minutes with the number of suppressed repeats.
Recovery is logged at info level.
