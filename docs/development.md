# Development

## Build, check, test

```bash
cargo build                       # debug build
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release --locked    # what install.sh builds
```

The minimum Rust version is `rust-version` in `Cargo.toml`. `Cargo.lock` is
committed and builds use `--locked`.

## Run locally (no install)

```bash
cargo run -- --listen 127.0.0.1:9393 --db /tmp/hwstats-dev/hwstats.db
```

As a normal user, root-only sources (RAPL energy) report `unavailable`.
Everything else works.

**Frontend iteration.** Set `HWSTATS_WEB_DIR` to the `web/` folder. The
binary then reads the known asset files from disk on every request, so a
browser reload shows edits without recompiling:

```bash
HWSTATS_WEB_DIR=$PWD/web cargo run -- --listen 127.0.0.1:9393 --db /tmp/hwstats-dev/hwstats.db
```

New files under `web/` must also be added to `ASSETS` in
`src/http/assets.rs`.

## Fixture trees

All `/proc` and `/sys` access goes through `util::sysfs::Roots`. Set
`HWSTATS_PROC_ROOT` and `HWSTATS_SYS_ROOT` to run the whole service against
fake trees. The fixtures live in `tests/fixtures/<machine>/{proc,sys}`:

| Fixture | Content |
|---|---|
| `laptop` | A laptop layout: two NVMe and two DIMM sensor chips, hp-wmi fans, a battery chip and an amdgpu chip (both skipped), a −273 °C sensor, an unreadable sensor, RAPL zones, a runtime-suspended NVIDIA dGPU. |
| `vm` | `/proc/stat` and `/proc/cpuinfo` without cpufreq (frequency fallback). |
| `rhel8` | `meminfo` without `MemAvailable`, no PSI, a battery using the charge/current interface. |
| `intel-desktop` | coretemp and an nct6798 chip with a 65535 fan reading. |

Unit tests use `Roots::fixture("<name>")`. Empty directories carry a `.keep`
file so the symlink targets exist after a clone.

## Adding a collector

1. Create `src/collectors/<name>.rs` with a struct holding `Roots`, a
   `DescCache` and any state, and implement `Collector::collect`. It must:
   - never panic on odd data; skip what cannot be read;
   - return `Err` only when the whole source failed for this tick;
   - set `out.health`: `ok`, `degraded` with a reason when using a fallback,
     or `unavailable` with a reason and, where useful, a hint;
   - use stable keys (see [collectors.md](collectors.md#stable-identity)) and
     one of the documented units;
   - turn counters into rates with `util::counter` (gap and reset handling).
2. Register it in `collectors::all()` and add its default interval and
   label to `COLLECTORS` in `src/settings.rs`.
3. Add fixtures and tests for the parser and for missing sources.
4. Show it in the page (`web/cards.js`) if it needs its own panel.
5. Document it in `docs/collectors.md` (and other docs it affects).

## Code layout

See [architecture.md](architecture.md). Conventions:

- No `unwrap()` on runtime data. Mutex poisoning is recovered with
  `unwrap_or_else(|e| e.into_inner())`.
- Logs that can repeat use `warn_limited!` (`src/util/ratelimit.rs`).
- Text from the system (labels, process names) is inserted in the page with
  `textContent` only.
