# Settings

There are two kinds of settings:

- **Bootstrap configuration:** `/etc/hwstats/config.toml`, read at start-up.
- **Runtime settings:** changed in the page and stored in the database. They
  apply immediately without a restart.

## Bootstrap configuration (`config.toml`)

| Key | Default | Description |
|---|---|---|
| `listen` | `"127.0.0.1:9393"` | Address and port of the page. |
| `database` | `"/var/lib/hwstats/hwstats.db"` | SQLite file. Its folder is the state directory (lock, NVML marker). |
| `backup_dir` | `<database folder>/backups` | Backup folder. |
| `allowed_hosts` | `[]` | Extra Host header names accepted. `"*"` accepts any. When listening on `0.0.0.0` without entries, any name is accepted and a warning is logged. |
| `log_level` | `"info"` | `error`, `warn`, `info` or `debug`. `RUST_LOG` overrides it. |

Unknown keys or invalid values produce a warning and the defaults. The
command-line options `--listen`, `--db` and `--backup-dir` override the file
(`--config` selects another file). Restart the service after editing:
`sudo systemctl restart hwstats`.

## Runtime settings

Stored as one JSON document (`settings` table) and exposed through
`GET`/`PUT /api/settings`. Every value is clamped to its range. Unknown
fields are ignored, and missing fields take defaults.

### `collectors.<name>`

| Field | Default | Range |
|---|---|---|
| `enabled` | `true` | |
| `interval_ms` | per collector | 250 – 60000 |

Default intervals:

| Collectors | Interval |
|---|---|
| `cpu` | 1 s |
| `system`, `pressure`, `memory`, `sensors`, `power`, `gpu_nvidia`, `gpu_drm` | 2 s |
| `disk`, `network`, `processes` | 3 s |
| `battery`, `thermal` | 5 s |
| `filesystem` | 30 s |

### `history`

| Field | Default | Range | Meaning |
|---|---|---|---|
| `resolution_s` | 10 | 1 – 300 | Width of each averaged point in the raw tier. |
| `raw_retention_hours` | 48 | 1 – 720 | How long raw points are kept. |
| `minute_retention_days` | 14 | 1 – 365 | How long 1-minute rollups are kept. |
| `hour_retention_days` | 365 | 1 – 3650 | How long 1-hour rollups are kept. |
| `max_db_mb` | 2048 | 64 – 1048576 | Size cap; the oldest data is removed first. |

### `backups`

| Field | Default | Range |
|---|---|---|
| `enabled` | `true` | |
| `keep_daily` | 7 | 1 – 365 |
| `keep_weekly` | 4 | 0 – 520 |
| `max_total_mb` | 1024 | 16 – 1048576 |

### `display`

| Field | Default | Values |
|---|---|---|
| `live_window_s` | 300 | 60 – 900 (the page offers 1, 5 and 15 minutes) |
| `smoothing` | `medium` | `off`, `low`, `medium`, `high` |
| `theme` | `auto` | `auto` (follow the system), `light`, `dark` |
| `temp_unit` | `C` | `C`, `F` (also switchable from the Temperatures card) |

### `network`

| Field | Default | Meaning |
|---|---|---|
| `exclude` | `["lo", "veth*", "docker*", "br-*", "virbr*", "vnet*", "cni*", "flannel*"]` | Interface name globs (`*` matches anything) that are not collected. At most 64 entries. |

## Example

```bash
curl -s -X PUT -H 'Content-Type: application/json' \
  -d '{"collectors": {"network": {"interval_ms": 5000}}, "display": {"theme": "dark"}}' \
  http://127.0.0.1:9393/api/settings
```
