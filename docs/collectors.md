# Collectors

Each collector reads one family of sources. Collectors try generic Linux
interfaces first and use hardware-specific methods only as enrichment or
fallback. A source that does not exist produces no series, and its panel is
hidden. Every tick reports a health state shown in Settings → Status and by
`hwstats doctor`.

| State | Meaning |
|---|---|
| `ok` | Primary source works. |
| `degraded` | Working through a fallback or some reads failed (the reason is shown). |
| `unavailable` | No source on this machine. It is re-probed every 30 s. |
| `disabled` | Turned off in settings. |
| `stalled` | A tick exceeded its time budget. No new tick starts until it returns. |
| `error` | The source failed or the collector panicked. It is retried with backoff. |

## Collector list

| Name | Default interval | Sources (fallbacks) | Series keys |
|---|---|---|---|
| `cpu` | 1 s | `/proc/stat`; `cpufreq/scaling_cur_freq` (fallback `/proc/cpuinfo`) | `cpu.usage`, `cpu.{user,nice,system,iowait,irq,softirq,steal}`, `cpu.core.<n>.{usage,mhz}`, `cpu.mhz_avg`, `cpu.mhz_max`, `cpu.ctxt`, `cpu.intr`, `cpu.procs_running`, `cpu.procs_blocked` |
| `system` | 2 s | `/proc/loadavg`, `/proc/uptime`, `/proc/sys/fs/file-nr`; info from `platform_profile`, cpufreq governor, driver, EPP, boost | `load.{1,5,15}`, `sys.threads`, `sys.uptime`, `sys.open_files` |
| `pressure` | 2 s | `/proc/pressure/{cpu,memory,io,irq}` (avg10) | `psi.<resource>.{some,full}` |
| `memory` | 2 s | `/proc/meminfo` (estimate when `MemAvailable` is missing), `/proc/vmstat` | `mem.{total,used,available,free,cached,used_pct,shmem,dirty,writeback,anon,slab}`, `swap.{total,used,used_pct,in,out}`, `mem.{majfault,pagein,pageout}` |
| `disk` | 3 s | `/proc/diskstats` (14/18/20-field variants), `/sys/block/*` | `disk.<id>.{read_bps,write_bps,read_iops,write_iops,util,await,inflight}`, `disk.total.{read_bps,write_bps}` |
| `filesystem` | 30 s | `/proc/self/mountinfo` + `statvfs` | `fs.<mount>.{total,used,avail,used_pct,inodes_pct}` |
| `network` | 3 s | `/proc/net/dev`, `/proc/net/wireless`, `/proc/net/sockstat{,6}`, `/sys/class/net/*` | `net.<if>.{rx_bps,tx_bps,rx_pps,tx_pps,errors,drops,up,wifi_quality,wifi_dbm}`, `net.total.{rx_bps,tx_bps}`, `net.{tcp,udp}.inuse`, `net.tcp.tw` |
| `sensors` | 2 s | every `/sys/class/hwmon/hwmon*` chip | `hwmon.<chip>.<device-id>.<type><n>` |
| `thermal` | 5 s | `/sys/class/thermal/thermal_zone*` not already in hwmon | `thermal.<type>[.<n>]` |
| `power` | 2 s | powercap RAPL `energy_uj` (fallback: hwmon `zenpower`, `amd_energy`, `fam15h_power`) | `power.rapl.<zone>[.<subzone>]`, `power.hwmon.*` |
| `battery` | 5 s | `/sys/class/power_supply/*` | `bat.<name>.{capacity,power,power_signed,voltage,energy,energy_full,health,cycles,time_left}`, `psu.<name>.{online,contract_w}` |
| `gpu_nvidia` | 2 s | NVML via `libnvidia-ml.so.1` (fallback `nvidia-smi`, 2 s timeout) | `gpu.<pci>.{active,util,mem_util,vram_used,vram_total,temp,power,power_limit,clock_gr,clock_sm,clock_mem,clock_video,fan,enc,enc_sessions,enc_fps,enc_latency,dec,pcie_rx,pcie_tx,procs}`; info facts and a per-process list (see below) |
| `gpu_drm` | 2 s | `/sys/class/drm/card*/device` + the card's hwmon | `gpu.<pci>.{active,util,mem_util,video,vram_used,vram_total,gtt_used,gtt_total,clock_gr,clock_mem,clock_fabric,temp,power,ppt,voltage,voltage_soc}` |
| `processes` | 3 s | `/proc/[pid]/stat`, `/proc/[pid]/cmdline`, `/etc/passwd` | `proc.{count,threads,running,blocked,zombie}`; top 15 applications by CPU and by memory, and every process for the details overlay (live only; see [Applications](#applications)) |

The derived `power_summary` series are described in [Derived series](#derived-series).

Units are part of each descriptor:

| Unit | Meaning |
|---|---|
| `%` | Percent |
| `C` | Degrees Celsius |
| `rpm` | Revolutions per minute |
| `W` | Watts |
| `V` | Volts |
| `A` | Amperes |
| `MHz` | Megahertz |
| `B` | Bytes |
| `B/s` | Bytes per second |
| `/s` | Events per second |
| `ms` | Milliseconds |
| `s` | Seconds |
| `Wh` | Watt-hours |
| `count` | Plain count |
| `bool` | 0 or 1 |
| `dBm` | Signal strength |

## Stable identity

Series keys must stay the same across reboots, so history is never spliced
between different devices.

- **hwmon.** `hwmonN` numbers change at boot. The key uses the chip name, a
  device id taken from the resolved `device` link and the channel (`temp1`,
  `fan2`, ...). The device id is the i2c address (`1-0050`), else the last
  PCI address in the path (`0000:02:00.0`), else the platform device name
  (`hp-wmi`). Duplicate names and addresses get a `-2` suffix.
- **Disks.** The key uses model + the last 6 characters of the serial, else
  the WWID, else the kernel name. Device-mapper volumes use `dm_<name>`.
- **Filesystems.** The key uses the mount point (`/` is `root`).
- **Network interfaces.** The key uses the interface name.
- **GPUs.** The key uses the PCI address.
- **Threads.** Per-thread series keep the logical CPU number in the key
  (`cpu.core.<n>.*`); their group is the physical core from
  `/sys/devices/system/cpu/cpu<n>/topology` (`core<id>`, or
  `pkg<p>.core<id>` with several packages), so SMT siblings are shown
  together.

## Rates and sanity checks

- Counters become rates using the monotonic clock. A rate is dropped when a
  counter goes backwards (reset or re-plug) or when more than 3× the
  interval has passed (suspend, stalled collector), instead of drawing a
  spike.
- RAPL energy wraps at `max_energy_range_uj`; the wrap is handled.
- Implausible values are discarded: non-finite numbers, temperatures
  outside −50…200 °C, fan readings of 65535 or more, and similar per-type
  ranges.
- A failing sensor file (EIO, ENODATA, ...) skips only that sensor for that
  tick.
- **Slow sensors.** An hwmon sensor whose read takes more than 50 ms (for
  example fans read through firmware/WMI, such as `hp-wmi`, about 300 ms per
  read) is read only every 10 s from then on. Its descriptor carries
  `period_ms`, so gap detection and live charts use that period. The
  collector's status shows how many sensors are slow.

## Hardware-specific behaviour

- **Laptop dGPU.** Both GPU collectors read the PCI
  `power/runtime_status` first. While it is `suspended` the GPU is shown as
  sleeping and is not queried, so monitoring never wakes it up.
- **AMD APU.** The integrated GPU's hwmon `PPT` sensor is the whole SoC
  package (CPU + iGPU) and is exported as `gpu.<pci>.ppt`, not as GPU
  power. An AMD GPU with at most 2 GiB of VRAM is considered integrated.
- **RAPL.** `energy_uj` is readable by root only. The service gets
  `CAP_DAC_READ_SEARCH` to read it. When run as a normal user, the `power`
  collector reports `unavailable` and the power summary falls back to PPT.
- **NVIDIA details (NVML only).**
  - Info facts: CUDA cores, architecture, compute capability, maximum core
    clock, memory bus width, current and maximum PCIe link, performance
    state, active clock limiters ("power limit", "thermal (hardware)", ...),
    driver, CUDA and VBIOS versions.
  - `extra` (per GPU): the processes using the GPU. Each entry has the pid,
    name (from `/proc/<pid>/comm`), compute/graphics type, GPU memory and
    the latest SM, memory, encoder and decoder utilization.
  - The `nvidia-smi` fallback provides only the basic figures.
- **NVML.** It is loaded at run time. A missing library, a driver/library
  version mismatch (after a driver update) or a driver that is not loaded
  makes the collector use `nvidia-smi` and retry NVML every 5 minutes. See
  [reliability.md](reliability.md) for the crash guard.
- **Batteries.** Both the `energy_*`/`power_now` and the
  `charge_*`/`current_now` interfaces are supported; power is derived from
  current × voltage when needed. Peripheral batteries (`scope=Device`) are
  ignored.
- **Filesystems.** Only local filesystem types are queried (ext2/3/4, xfs,
  btrfs, vfat, exfat, ntfs, f2fs, zfs and a few others). Network, FUSE and
  automount filesystems are skipped, so a hung server cannot block the
  collector. Mounts under `/snap`, `/run` and container storage are ignored.
- **Network.** Interfaces matching `network.exclude` (default `lo`, `veth*`,
  `docker*`, `br-*`, `virbr*`, `vnet*`, `cni*`, `flannel*`) are skipped.
  Totals include physical interfaces only.

## Applications

The `processes` collector groups processes into applications by the
executable name in `argv[0]`.

- **Rewritten command lines.** Chromium and Electron programs rewrite their
  command line into one space-separated string; only the part before the
  first option counts.
- **Process titles.** Titles such as `nginx: worker process` keep the part
  before `: `.
- **Kernel threads** (no command line) form one "kernel threads" group.

Multi-process programs also get a type per process:

- **Chromium/Electron** (`--type=`): main process, renderer (tabs,
  frames), extension, GPU process, utility services (network, audio,
  storage, ...), zygote, crash reporter, broker.
- **Firefox** (`-contentproc`, the last argument): content (tabs), media
  decoder, GPU process, network, utility, fork server.

The application name, command (up to 400 characters), user and type are
read once per process lifetime. Each tick then only reads
`/proc/[pid]/stat`.

The collector sends two kinds of data:

- **Live (`extra`):** for the top 15 applications by CPU and by memory,
  their process count, summed CPU (100% = one thread), resident memory,
  threads, users, a `multi` flag and the pids.
- **On request (`detail`):** every process, served by `/api/processes`.

The details of a single process (`/api/process`) are not part of the
collector: `src/procinfo.rs` reads `/proc/<pid>` when the page asks for
them. See [api.md](api.md#get-apiprocesspidpid).

Memory is the resident set size (RSS). Memory shared between processes of
one application is counted in each of them. The tab a renderer belongs to is
not visible from `/proc`.

## Derived series

`src/derived.rs` publishes `power.total`, `power.cpu_total`,
`power.gpu_total` and `power.dram_total` every power interval. These sums use
only components with recent values:

- **CPU package:** RAPL package zones, else the APU PPT, else hwmon CPU power
  sensors.
- **GPUs:** NVIDIA power and discrete AMD/Intel GPU power. An APU's PPT is
  never added here, because it is already counted in the CPU package.
- **Memory:** RAPL DRAM zones. These exist on Intel platforms only.

The info text `parts` lists what the total includes. Screens, SSDs and other
parts are not measured. On battery, `bat.<name>.power` is the whole-system
draw.

## Adding a collector

See [development.md](development.md#adding-a-collector).
