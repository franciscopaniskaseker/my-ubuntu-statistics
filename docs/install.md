# Installation

## Supported systems

| Family | Versions |
|---|---|
| Ubuntu | 22.04, 24.04 |
| RHEL, Rocky Linux, AlmaLinux, CentOS Stream, Oracle Linux | 8, 9, 10 |

Other Debian- or Fedora-like systems are attempted with a warning.
Architectures: x86_64 and aarch64.

## `install.sh`

Run it as your normal user from the repository folder; sudo is used for
system changes. Running it as root also works: then it builds as the user
in `SUDO_USER`, or as root.

| Command | Effect |
|---|---|
| `./install.sh` | Build and install, or upgrade an existing install. |
| `./install.sh --build-only` | Only build `target/release/hwstats`. |
| `./install.sh --rollback` | Go back to the binary installed before the last upgrade. |
| `./install.sh --uninstall` | Remove binary, unit and docs; keep data and config. |
| `./install.sh --purge [--yes]` | Also remove `/var/lib/hwstats`, `/etc/hwstats` and the `hwstats` user. |

`--uninstall` also removes `hwstats.previous`.

Steps (each one names itself if it fails, with a hint):

1. **Check the system.**
   - Distribution and version from `/etc/os-release`, and the architecture.
   - About 2 GB free for the build, 500 MB free in `/var/lib`.
   - Whether systemd is running. Without it (containers, WSL) the binary is
     installed but no service is created.
   - Whether the configured port is free, or already used by hwstats itself.
2. **Install build dependencies**, only the missing ones:
   - Ubuntu: `build-essential`, `curl`, `ca-certificates`.
   - RHEL family: `gcc`, `curl`, `ca-certificates`. `curl` is only installed
     when no `curl` command exists, because RHEL 9/10 ship `curl-minimal`,
     which conflicts with `curl`.
   - If no enabled repository is detected (an unregistered RHEL), the script
     warns and explains what to do if the package installation fails.
3. **Rust.** The script uses an existing `cargo` that meets `rust-version`
   from `Cargo.toml` (currently 1.89). Otherwise it installs rustup for the
   build user in `~/.cargo` (minimal profile, stable, without changing shell
   profiles) or updates an existing rustup. If `/tmp` is mounted `noexec`,
   temporary files go to `~/.cache/hwstats-build`.
4. **Build.** `cargo build --release --locked`, as the build user, never as
   root. It needs internet access to crates.io the first time.
5. **Install.**
   - Creates the `hwstats` system user.
   - Stops a running service.
   - Installs the binary with `install` so it gets the correct SELinux
     label, and runs `restorecon` when available.
   - Copies `config.toml` only if none exists yet.
   - Installs the docs and the unit, then runs `daemon-reload`, `enable` and
     `restart`.
6. **Check.** Waits up to 15 s for `/api/health` and checks that the service
   is still active a few seconds later; otherwise it prints the journal.
   Then it runs `hwstats doctor` and prints the URL.

Running the script again is safe and performs an upgrade (see below).

## Upgrading

Pull or copy the new version into the repository folder and run
`./install.sh` again. When hwstats is already installed (the binary or the
unit exists), the script upgrades instead of installing.

**Kept:**

- history and backups in `/var/lib/hwstats`;
- the settings changed in the page (stored in the database);
- `/etc/hwstats/config.toml`;
- local unit changes made with `sudo systemctl edit hwstats` (drop-ins in
  `/etc/systemd/system/hwstats.service.d/`).

**Steps:**

1. After the build, and before anything installed is touched, it prints the
   installed and the new version.
2. It makes a backup of the database with the installed binary
   (`hwstats backup`, run as the `hwstats` user while the service keeps
   running). This backup becomes the day's backup in
   `/var/lib/hwstats/backups/`. If it fails, the upgrade continues; schema
   migrations take their own backup.
3. It keeps the installed binary as `/usr/local/bin/hwstats.previous`.
4. It stops the service, installs the new binary, unit and docs, and starts
   it again.
   - `config.toml` is never overwritten.
   - The shipped defaults are stored as `/etc/hwstats/config.toml.default`.
     When they differ from the previous version's defaults, the script says
     so, so you can compare them with your file.
5. It runs the usual checks: health endpoint, service still active,
   `hwstats doctor`.
6. If the new version does not start, it **rolls back automatically** to
   `hwstats.previous` and restarts the service.

`./install.sh --rollback` does the same rollback by hand at any time.

If a newer version changed the database schema, the previous binary refuses
to write to the database (it runs in memory-only mode). In that case,
restore the pre-upgrade backup ([backups.md](backups.md#restore)).

## The service

| Item | Value |
|---|---|
| Unit | `/etc/systemd/system/hwstats.service` (`packaging/hwstats.service`) |
| User | `hwstats` (system user, no login) |
| State | `/var/lib/hwstats` (`StateDirectory=`) |
| Capabilities | `CAP_DAC_READ_SEARCH` only, to read root-only counters such as RAPL energy |

Hardening:

- `NoNewPrivileges`, `ProtectSystem=full`, `ProtectHome=read-only`.
- `PrivateDevices` is not used, because NVIDIA needs `/dev/nvidia*`.
  `ProtectProc` is not used, because the collectors read `/proc`.
- `Type=notify` with `WatchdogSec=60`, `Restart=always` and no start limit.

Common commands:

```bash
systemctl status hwstats
journalctl -u hwstats -f
sudo systemctl restart hwstats
hwstats doctor
```

## `hwstats doctor`

When the service is reachable, it prints the service's own view: each
collector's state, source, reason and hints, plus the database and backup
summary. When the service is not running, it probes all collectors locally
as the current user. Those results can differ from the service; for
example, RAPL needs the service capability.

Typical hints:

| Hint | Meaning |
|---|---|
| `psi=1` | On RHEL 8 pressure information is disabled until you add the kernel boot parameter `psi=1`. |
| `drivetemp` | SATA disk temperatures appear after `sudo modprobe drivetemp`. To load it at boot, add it to `/etc/modules-load.d/`. |
| `sensors-detect` | Desktop motherboard sensors (Super I/O chips such as nct6775 or it87) may need `sensors-detect` from lm-sensors to find the right driver. |
| NVIDIA | NVIDIA statistics need the proprietary driver; after a driver update, reboot to fix a driver/library mismatch. |

The installer never changes kernel modules or boot parameters itself.

## Troubleshooting

- **Port in use:** set another `listen` in `/etc/hwstats/config.toml` and
  restart.
- **Build fails:** check `cargo --version` (needs 1.89 or newer), internet
  access and free disk space. Run `./install.sh --build-only` to retry only
  the build.
- **Page says history is off:** see Settings → Status for the database
  error, for example a read-only or full disk.
- **Corrupt database:** handled automatically at start-up; see
  [storage.md](storage.md#recovery).
- **Process details say "hidden":** the kernel shows the executable,
  working directory, open files and disk I/O of a process only to its owner
  and root, and the service runs as `hwstats`. To show them for every
  process, give the service `CAP_SYS_PTRACE` with a drop-in
  (`sudo systemctl edit hwstats`):

  ```ini
  [Service]
  AmbientCapabilities=CAP_SYS_PTRACE
  CapabilityBoundingSet=CAP_SYS_PTRACE
  ```

  Then run `sudo systemctl restart hwstats`. This capability lets the
  service read the memory of any process, which weakens its isolation; the
  default unit does not grant it.
