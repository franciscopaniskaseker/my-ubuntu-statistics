# hwstats

> **Personal project.** I wrote hwstats for my own laptop. With the help of
> Claude (Claude Code) I am making it available for Ubuntu and Red Hat based
> systems, but it is **not meant for production use**. It comes with no
> warranty or support: use it at your own risk.

hwstats is a small local web page that shows every hardware statistic of
the machine it runs on, live and with history, on a single page. It runs as
a systemd service, stores history in a single SQLite file and serves the
page itself on `http://127.0.0.1:9393/` (no web server needed).

## What it shows

- **Processor:** usage split into user, system, I/O wait and other; load
  bars for every thread, grouped by physical core (yellow at 70 %, red at
  90 %); clock speeds; package temperature and power.
- **Load and pressure:** load averages, runnable and blocked tasks, and
  pressure stall information (PSI) for CPU, I/O and memory.
- **Memory:** used, available, cache and swap; paging activity.
- **GPUs:** NVIDIA (through NVML, or `nvidia-smi` as a fallback) and
  AMD/Intel through DRM sysfs: load, video engine, temperature, power, clocks,
  video and shared memory. NVIDIA cards also show encoder sessions, CUDA
  cores, PCIe link and traffic, clock limiters and the processes using the
  GPU. A sleeping laptop dGPU is never woken up to be measured.
- **Temperatures:** every hwmon sensor and thermal zone as numbers, grouped by
  chip; °C by default, °F selectable.
- **Storage:** per-disk throughput, IOPS, busy time and latency; filesystem
  usage.
- **Network:** per-interface traffic, Wi-Fi signal and quality, socket counts.
- **Power and battery:** CPU package, GPU and memory power where the
  hardware reports them, a measured total, battery charge, draw, health and
  cycles.
- **Fans:** RPM of every fan the kernel exposes.
- **Processes:** applications grouped by program (a browser's many
  processes count as one), by CPU or by memory. Select one to see every
  process, with a breakdown by type (tabs, extensions, GPU, services) for
  browsers and Electron apps. Select a process for its details from
  `/proc`: CPU and memory trends, memory breakdown, disk I/O, open files,
  systemd unit, command line, parent, children and threads.
- **System:** uptime, power profile, CPU governor and energy preference.

Collectors use generic Linux interfaces (`/proc`, `/sys/class/hwmon`,
`/sys/class/power_supply`, powercap, DRM), so the page adapts to the
machine: panels for missing hardware simply do not appear.

Numbers glide to new values, and live charts scroll continuously instead of
jumping. A settings overlay (top right) changes how often each statistic is
read, how long history is kept, the backup policy and display options.

## Install

```bash
git clone <this repository> hwstats
cd hwstats
./install.sh
```

The script asks for your sudo password when needed. It does the following:

1. installs the few build packages that are missing (a C compiler, curl);
2. installs Rust with rustup if your Rust is missing or too old;
3. builds the release binary;
4. installs the `hwstats` systemd service and starts it.

Then open <http://127.0.0.1:9393/>.

Supported systems: Ubuntu 22.04 and 24.04, and RHEL, Rocky, AlmaLinux,
CentOS Stream and Oracle Linux 8, 9 and 10. See
[docs/install.md](docs/install.md) for details and troubleshooting.

Check what is available on your machine:

```bash
hwstats doctor
```

## Upgrade

Get the new version into the same folder (for example `git pull`) and run
`./install.sh` again. History, backups, settings and
`/etc/hwstats/config.toml` are kept. The script backs up the database first,
keeps the previous binary, and rolls back automatically if the new version
does not start; `./install.sh --rollback` returns to the previous version
by hand. See [docs/install.md](docs/install.md#upgrading).

## History and backups

History is stored in `/var/lib/hwstats/hwstats.db` at three resolutions:

| Resolution | Kept for |
|---|---|
| 10-second averages | 48 hours |
| 1-minute averages | 14 days |
| 1-hour averages | 365 days |

The database has a 2 GB size cap. All of these limits can be changed in Settings.

A compressed backup is made every day into `/var/lib/hwstats/backups/`. By
default it keeps the last 7 daily and 4 weekly backups, up to 1 GB in total.
To restore one:

```bash
sudo systemctl stop hwstats
sudo -u hwstats hwstats --config /etc/hwstats/config.toml restore /var/lib/hwstats/backups/hwstats-<date>.db.gz
sudo systemctl start hwstats
```

See [docs/backups.md](docs/backups.md).

## Uninstall

```bash
./install.sh --uninstall   # keeps history, backups and configuration
./install.sh --purge       # removes everything
```

## Documentation

- [Architecture](docs/architecture.md)
- [Collectors](docs/collectors.md)
- [Storage](docs/storage.md)
- [Backups](docs/backups.md)
- [Reliability](docs/reliability.md)
- [HTTP API](docs/api.md)
- [Web page](docs/frontend.md)
- [Settings](docs/settings.md)
- [Installation](docs/install.md)
- [Development](docs/development.md)

## Third-party components

- [uPlot](https://github.com/leeoniya/uPlot) charts (MIT license), in
  `web/vendor/`.
- [Barlow](https://github.com/jpt/barlow) typeface (SIL Open Font License),
  in `web/vendor/fonts/`.
- Rust crates listed in `Cargo.toml`, notably rusqlite with a bundled SQLite.
