# Backups

## Schedule

The backup scheduler runs inside the service. It checks two minutes after
start, then every hour. A backup is made when backups are enabled and the
newest backup is more than 24 hours old. This also works on a laptop that is
often off: the backup happens soon after it is switched on, not at a fixed
time. After a failure the next attempt is 30 minutes later.

Backups are also made in these cases:

- before a schema migration;
- before an upgrade with `install.sh`, which also keeps the previous binary;
- when you press **Back up now** in Settings → Backups;
- when you run `hwstats backup`.

## How a backup is made

1. Check free space: the backup folder needs at least twice the database
   size free, otherwise the backup is skipped and the error is shown in
   Settings.
2. `VACUUM INTO` a temporary file. This is a consistent, compact snapshot
   taken without stopping writes.
3. Verify the snapshot with `PRAGMA quick_check`.
4. Gzip it to `hwstats-<YYYYMMDDTHHMMSSZ>.db.gz.tmp`, fsync it and rename
   it into place. A half-written backup never looks complete.
5. Remove the temporary snapshot and apply rotation.

The work runs on a separate thread with CPU nice 19 and the idle I/O class.
When run as root (CLI), the files are given the owner of the database.

## Rotation

| Setting | Default | Effect |
|---|---|---|
| `backups.keep_daily` | 7 | The newest backup of each of the last N days is kept. |
| `backups.keep_weekly` | 4 | The newest backup of each of the last N weeks (Monday based, UTC) is kept. |
| `backups.max_total_mb` | 1024 | If the kept backups are larger than this, the oldest are deleted; the newest is always kept. |

Everything else is deleted. Several backups made on the same day collapse
into the newest one.

## Location

The default folder is `/var/lib/hwstats/backups/`. Change it with
`backup_dir` in `/etc/hwstats/config.toml`; the `hwstats` user must be able
to write there.

The default folder is on the same disk as the database. To keep copies
elsewhere, copy the `.db.gz` files with your usual backup tool. They are
complete, self-contained SQLite databases once decompressed.

## Restore

```bash
sudo systemctl stop hwstats
sudo -u hwstats hwstats --config /etc/hwstats/config.toml restore /var/lib/hwstats/backups/hwstats-20260101T030000Z.db.gz
sudo systemctl start hwstats
```

`restore` refuses to run while the service holds its lock. It does the following:

1. decompresses the backup next to the database;
2. verifies it;
3. moves the current database aside as `hwstats.db.pre-restore-<stamp>`;
4. moves the restored file into place.

Manual restore with standard tools:

```bash
sudo systemctl stop hwstats
cd /var/lib/hwstats
sudo mv hwstats.db hwstats.db.old; sudo rm -f hwstats.db-wal hwstats.db-shm
gunzip -c backups/hwstats-<stamp>.db.gz | sudo -u hwstats tee hwstats.db >/dev/null
sudo systemctl start hwstats
```

## Command line

```bash
sudo -u hwstats hwstats --config /etc/hwstats/config.toml backup   # make a backup now and rotate
```
