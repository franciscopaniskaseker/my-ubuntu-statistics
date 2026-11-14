# HTTP API

All endpoints are served by the binary on the `listen` address (default
`127.0.0.1:9393`). Responses are JSON unless noted. Requests must carry a
local Host header (see [reliability.md](reliability.md#network-edge)).

## Page and assets

| Method | Path | Description |
|---|---|---|
| GET | `/` | The page (`index.html`) with a strict Content-Security-Policy. |
| GET | `/assets/<path>` | Embedded CSS, JS, fonts and vendor files. |
| GET | `/favicon.ico` | The icon. |

## Data

### `GET /api/health`
`{"ok": true, "instance": "<id>", "uptime_s": 12, "version": "0.1.0"}`.
`instance` changes on every start, so clients can tell the service restarted.

### `GET /api/meta`
Host facts (`host`), the live series catalog (`series`), the collector list
with labels and default intervals (`collectors`), `catalog_version`,
`history_available` (false in memory-only mode) and the server clock
(`now_ms`).

Each series descriptor has this shape:

```json
{"key": "hwmon.nvme.0000-02-00.0.temp1", "collector": "sensors", "metric": "temp",
 "group": "nvme.0000-02-00.0", "group_label": "NVMe Example SSD 2TB",
 "label": "Composite", "unit": "C", "crit": 84.85, "max": 81.85}
```

`crit` and `max` are present when the sensor reports limits. `period_ms` is
present when a series is sampled less often than its collector interval
(slow sensors).

### `GET /api/snapshot`
The latest value of every series and the latest text facts and process list:

```json
{"catalog_version": 17, "series": {"cpu.usage": [1794504528897, 14.88]},
 "info": {"system": {"governor": "powersave"}}, "processes": {...},
 "extra": {"gpu_nvidia": {"0000-01-00.0": [{"pid": 2735, "name": "Xorg", ...}]}}}
```

`processes` holds the application summary (`count`, `threads`, `apps`,
`apps_cpu`, `apps_mem`; each application has `name`, `count`, `cpu`, `rss`,
`mem`, `threads`, `users`, `multi` and `pids`). `extra` holds the free-form
data of other collectors, currently the per-GPU process list of
`gpu_nvidia`.

### `GET /api/processes?app=<name>`
Every process from the latest tick, or only those of one application. Each
entry has `pid`, `ppid`, `app`, `kind` (process type or `null`), `user`,
`cpu`, `rss`, `mem`, `threads`, `state`, `age_s` and `cmd`. Status 503 means
no data has been collected yet.

Times are Unix milliseconds.

### `GET /api/process?pid=<pid>`
Details of one process, read from `/proc/<pid>` when requested. Every field
is optional; `null` means the value is missing or not readable.

| Field | Content |
|---|---|
| `pid`, `ppid`, `pgrp`, `session`, `start_ticks` | Identity. `start_ticks` (start time in clock ticks) tells a reused PID apart. |
| `comm`, `app`, `kind`, `state`, `kernel_thread` | Kernel name, application, process type, state letter. |
| `uid`, `user`, `gid` (`euid`, `euser` when different) | Owner. |
| `started_ms`, `age_s` | Start time (Unix ms) and running time. |
| `cpu` | CPU % from the latest processes tick (100% = one thread). |
| `cpu_s`, `user_s`, `system_s`, `children_cpu_s` | CPU time in seconds. |
| `nice`, `priority`, `policy`, `rt_priority`, `processor`, `cpus_allowed` | Scheduling; `processor` is the CPU it last ran on. |
| `threads`, `ctx_voluntary`, `ctx_involuntary` | Thread count and context switches. |
| `mem` | Bytes: `rss`, `anon`, `file`, `shmem`, `virt`, `peak_virt`, `peak_rss`, `data`, `stack`, `exe`, `lib`, `pte`, `swap`, `locked`. |
| `io` | `/proc/<pid>/io` counters (`read_bytes`, `write_bytes`, `rchar`, `wchar`, `syscr`, `syscw`, `cancelled_write_bytes`). |
| `fds`, `fd_limit`, `fd_limit_hard` | Open file descriptors and the "Max open files" limits (`null` = unlimited). |
| `exe`, `cwd` | Executable and working directory. |
| `oom_score`, `oom_score_adj` | Out-of-memory killer score and adjustment. |
| `cgroup`, `unit` | Control group path and its systemd unit (`.scope`/`.service`). |
| `seccomp`, `no_new_privs` | Sandbox flags from `/proc/<pid>/status`. |
| `args`, `args_truncated` | Command line, one entry per argument (up to 512 arguments of 4096 characters). Rewritten single-string command lines are split before each `--option`. |
| `thread_list`, `threads_total` | Up to 256 threads, busiest first: `tid`, `name`, `state`, `cpu_s`, `processor`. |
| `parent`, `children` | From the latest processes tick: `pid`, `app`, `kind`, `user`, `cpu`, `rss`, `threads`, `state` (up to 200 children, busiest first). |
| `restricted` | Fields the kernel refused to the service (`exe`, `cwd`, `io`, `fds`). |
| `now_ms` | Time of the read. |

The kernel shows `exe`, `cwd`, `io` and `fd` only to the process owner and
root, so for processes of other users they are `null` and listed in
`restricted`. `environ` is never read. Errors are JSON `{"error": ...}`:
404 when the process does not exist, 400 for an invalid pid, 503 when four
reads are already in progress, 504 when the read takes longer than 3 s (a
process stuck in the kernel).

### `GET /api/live?keys=a,b&since_ms=<ms>`
In-memory ring contents (up to 15 minutes, full sampling rate):
`{"now_ms": ..., "series": {"a": {"t": [ms...], "v": [...]}}}`. Up to 400
keys per request.

### `GET /api/history?keys=a,b&from=<s>&to=<s>&points=<n>`
Bucketed history from the database tiers plus the ring. `from` and `to` are
Unix seconds (`to` defaults to now, `from` to one hour before `to`).
`points` is the target number of buckets (10–4000, default 600).

```json
{"from": 1794500000, "to": 1794503600, "step": 10,
 "series": {"a": {"t": [...], "avg": [...], "min": [...], "max": [...]}}}
```

`null` entries mark gaps. In memory-only mode only ring data is returned.
Status 503 means the database could not be read.

### `GET /api/stream`
Server-Sent Events. Limited to 32 concurrent clients (503 beyond that).

| Event | Data |
|---|---|
| `hello` | `{"instance", "catalog_version"}`, sent first. |
| `sample` | `{"c": collector, "t": ms, "v": {key: value}}`, one per collector tick. |
| `info` | `{"c": collector, "info": {...}}` when text facts change. |
| `processes` | Application summary (`apps_cpu`, `apps_mem`, `count`, `threads`, `apps`, `ts`). |
| `extra` | `{"c": collector, "data": {...}}`, free-form data of other collectors (GPU processes). |
| `catalog` | `{"version"}` when series appear or disappear; refetch `/api/meta`. |
| `settings` | The full settings document after a change. |
| `status` | One collector status when its state or reason changes. |
| `resync` | The client fell behind; refetch `/api/snapshot`. |

A keep-alive comment is sent every 15 s.

## Settings

### `GET /api/settings`
The full runtime settings document (see [settings.md](settings.md)).

### `PUT /api/settings`
Body: a partial document (JSON merge patch), for example
`{"collectors": {"disk": {"interval_ms": 5000}}}`. The result is validated,
clamped, stored and applied immediately; the response is the full document.
Status 400 is returned for invalid JSON or wrong types.

## Status and backups

### `GET /api/status`
- Overall state.
- Per-collector status: `name`, `state`, `source`, `reason`, `hint`,
  `interval_ms`, `series`, `last_ok_ms`, `last_duration_ms`, `errors`,
  `restarts`.
- Storage: path, sizes, rows written, failed or dropped batches, last error.
- `memory_only` and `storage_error`.
- Backup summary.
- Start-up `notes`.

### `GET /api/backups`
`{"dir", "backups": [{"file", "size", "created"}], "total_bytes", "last_error", "available"}`.

### `POST /api/backups`
Makes a backup now and applies rotation. Returns the new backup or an error
(409 in memory-only mode).
