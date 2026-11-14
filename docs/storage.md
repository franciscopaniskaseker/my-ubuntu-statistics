# Storage

History lives in one SQLite file (default `/var/lib/hwstats/hwstats.db`).
SQLite is compiled into the binary (rusqlite `bundled`), so there is no
database server and the version does not depend on the distribution.

## Connection settings

- `journal_mode=WAL`, `synchronous=NORMAL`, `busy_timeout=5000`.
- `auto_vacuum=INCREMENTAL` is set when the database is created. Pruning
  is followed by `PRAGMA incremental_vacuum` and
  `PRAGMA wal_checkpoint(TRUNCATE)`, so the files shrink again.
- The schema version is stored in `PRAGMA user_version`. Migrations run in
  a transaction, and a backup is taken before migrating an existing
  database. A database with a newer schema than the binary is not written
  to (the service runs in memory-only mode instead).

## Schema

| Table | Columns | Notes |
|---|---|---|
| `series` | `id`, `key` (unique), `collector`, `metric`, `grp`, `group_label`, `label`, `unit`, `first_seen`, `last_seen` | Descriptors. Labels are updated when they change. |
| `samples` | `series_id`, `ts`, `value` | Raw tier: averages over `history.resolution_s` seconds. `WITHOUT ROWID`, primary key `(series_id, ts)`. |
| `samples_1m` | `series_id`, `ts`, `avg`, `min`, `max` | 1-minute rollup. |
| `samples_1h` | `series_id`, `ts`, `avg`, `min`, `max` | 1-hour rollup. |
| `settings` | `key`, `value` | Runtime settings document (`key = 'settings'`). |
| `meta` | `key`, `value` | Rollup watermarks (`rollup_1m_until`, `rollup_1h_until`) and the hardware inventory (`hardware`, a JSON document; see [hardware.md](hardware.md)). |

Timestamps are Unix seconds (UTC). The raw tier labels each bucket with its
start time. Inserts are upserts, so a wall-clock jump cannot cause errors.

## Write path

- The hub accumulates every value. A ticker aligned to
  `history.resolution_s` drains the averages and queues one batch.
- The writer thread inserts each batch in one transaction. Its queue holds
  256 messages; when it is full the oldest batch is dropped and counted.
- Errors such as `SQLITE_BUSY`, `SQLITE_FULL` or I/O errors are logged
  (rate-limited) and shown in the status. The failed batch is dropped and
  the writer backs off (1 s up to 60 s). It never stops.
- On shutdown, pending values are flushed and the WAL is checkpointed.

## Rollups, retention and size cap

Every minute the writer aggregates complete minutes of raw data into
`samples_1m`, with a 30-second lag for late rows. Every hour it does the
following:

1. aggregates complete hours of 1-minute data into `samples_1h` (2-minute lag);
2. updates `series.last_seen`;
3. deletes data older than the retention of each tier, and series not seen
   within the 1-hour retention;
4. if the database is larger than `history.max_db_mb`, deletes the oldest
   quarter of the remaining time span, raw data first, then minute, then
   hour data, until it fits;
5. runs an incremental vacuum and a WAL checkpoint.

The first hourly run happens 5 minutes after start. All statements use the
`(series_id, ts)` primary key.

## Reading history

`/api/history` picks a starting tier from the requested range:

| Condition | Tier |
|---|---|
| Range of 6 hours or less, inside raw retention | Raw |
| Range of 7 days or less, inside minute retention | 1-minute |
| Otherwise | 1-hour |

Then:

- Buckets are `ceil(range / points)` seconds, rounded up to a multiple of
  the tier step.
- Each finer tier fills the time after the last bucket of the coarser one,
  because rollups lag behind. The in-memory ring fills whatever has not
  been written yet.
- Gaps longer than 2.5 buckets, or 3 sampling periods (the collector
  interval or the series `period_ms`), become `null`, so charts show breaks
  for suspend or downtime instead of straight lines.
- Values are rounded for transport: 3 decimals below 10, 2 below 1000,
  integers above.

## Recovery

At start-up the service runs `PRAGMA quick_check`. If the database is
corrupt, it does the following:

1. moves the file and its `-wal`/`-shm` files aside as
   `hwstats.db.corrupt-<stamp>`;
2. restores the newest backup that passes verification;
3. if no backup works, starts with an empty database.

What happened is shown as a note in the page and in Settings → Status.

If the database cannot be opened at all (read-only disk, permissions), the
service keeps running in **memory-only mode**. Live values and the last 15
minutes still work, and the page shows a banner.

## Inspecting the database

```bash
sudo sqlite3 -readonly /var/lib/hwstats/hwstats.db \
  "SELECT key, unit FROM series ORDER BY key LIMIT 20;"
```

(Install the `sqlite3` or `sqlite` package for the command-line tool.)
