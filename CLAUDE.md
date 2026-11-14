# CLAUDE.md

hwstats is a local hardware statistics dashboard. It is one Rust binary
(tokio + axum) that collects statistics from `/proc` and `/sys`, keeps
history in SQLite (bundled, single file) with daily gzip backups, and serves
an embedded single page (plain JS + uPlot) on `127.0.0.1:9393`. It runs as
the `hwstats` systemd service and is installed by `install.sh` on Ubuntu
22.04/24.04 and RHEL-family 8/9/10.

## Layout

```
src/main.rs           CLI (serve, doctor, backup, restore) and start-up
src/collectors/       one module per source + supervised scheduler (mod.rs)
src/hub.rs            latest values, rings, SSE broadcast, collector status
src/derived.rs        derived series (power summary)
src/storage/          schema, writer thread, rollups, queries, recovery
src/backup.rs         backups, rotation, restore
src/http/             routes, embedded assets, Host/Origin guard
src/procinfo.rs       on-demand details of one process (/api/process)
src/settings.rs       runtime settings; src/config.rs bootstrap config
src/util/             sysfs readers, counters, parsing, time, lock, logging
web/                  page (index.html, style.css, *.js, vendor/)
packaging/            systemd unit and default config.toml
tests/fixtures/       fake /proc and /sys trees used by unit tests
docs/                 documentation (see index below)
```

## Commands

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
cargo build --release --locked
HWSTATS_WEB_DIR=$PWD/web cargo run -- --listen 127.0.0.1:9393 --db /tmp/hwstats-dev/hwstats.db
./install.sh            # install or upgrade the service (needs sudo)
bash -n install.sh && systemd-analyze verify packaging/hwstats.service
```

## Documentation index

- `docs/architecture.md`: components, threads, data flow, files on disk.
- `docs/collectors.md`: every collector, sources, fallbacks, series keys, units.
- `docs/storage.md`: SQLite schema, tiers, rollups, retention, recovery.
- `docs/backups.md`: schedule, rotation, restore.
- `docs/reliability.md`: supervision, limits, watchdog, guards.
- `docs/api.md`: HTTP endpoints and SSE events.
- `docs/frontend.md`: page structure, panels, colors, smoothing.
- `docs/settings.md`: config.toml and runtime settings with defaults.
- `docs/install.md`: install.sh, service, doctor, troubleshooting.
- `docs/development.md`: building, fixtures, adding a collector.

Read the relevant doc before changing an area.

## Rules

1. **Commit after every small, self-contained task.** One logical change per
   commit, with an imperative subject line (for example "Add fan curve
   panel"). Before committing, run `cargo fmt`,
   `cargo clippy --all-targets -- -D warnings` and `cargo test`, and fix
   failures. Never commit `target/`, database files or backups.
2. **Keep the documentation current.** Every change to behaviour, settings,
   keys, endpoints, files or installation updates the matching `docs/*.md`
   (and `README.md` when users see it) in the same commit. A new area gets a
   new `docs/<area>.md` and an entry in the index above.
3. **Docs describe how things work.** No conversation history, decision
   logs or "we changed X because Y" narratives.
4. **Generic first.** Collectors use generic kernel interfaces first and add
   hardware-specific methods only as enrichment or fallback. A collector must
   never panic or block the service, must handle missing sources, and must
   report its health with a reason and a hint.
5. **Stability over features.** No `unwrap()` on runtime data, bounded
   memory and queues, rate-limited logs. The database and the network edge
   must never take the live view down.
6. **Keep this file short.** It must stay well under 120,000 characters
   (check with `wc -c CLAUDE.md`); put details in `docs/`.
7. **Verify on the real machine.** Run the service locally and look at the
   page in a browser after UI changes. For `install.sh` changes, run
   `bash -n`. Only paths that were actually run count as tested.
