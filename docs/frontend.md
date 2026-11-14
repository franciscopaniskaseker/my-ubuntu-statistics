# Web page

The page is plain HTML, CSS and ES modules in `web/`, with no build step.
The files are compiled into the binary (`src/http/assets.rs`), so the page
works offline. Charts use the vendored uPlot; text uses the embedded Barlow
and Barlow Condensed fonts.

## Files

| File | Role |
|---|---|
| `index.html` | Header (host plate, time range, connection state, settings button), grid, settings dialog. |
| `style.css` | Design tokens for light and dark themes, layout, panels, bars, dialog. |
| `app.js` | Start-up, data loading, SSE handling, banners, range switching, theme. |
| `cards.js` | Builds panels from the series catalog and routes samples to readouts, meters, bar groups and charts. |
| `charts.js` | Live store and the uPlot wrapper (live scrolling and history mode). |
| `vbar.js` | Grouped vertical bar charts (load per core): gridlines, value above or inside the bar, thread and core labels. |
| `tween.js` | Smooth number transitions. |
| `format.js` | Unit formatting (IEC bytes, °C/°F, GHz, durations). |
| `settings.js` | Settings overlay. |
| `procdetail.js` | Process overlay: application view (all processes of one application) and process view (details of one process). |

## Layout

- **Processor** spans the first line. Figures and the usage chart are on the
  left, the per-core bars on the right; below 1100 px they stack.
- **All other cards** are packed into balanced columns: three from 1240 px,
  two from 760 px, one below. Each card goes into the currently shortest
  column, so cards keep their own height and no space is left empty. The
  packing runs after each rebuild, after fonts load and when the window is
  resized.
- **Processes** spans the full width at the bottom.

## Panels

Panels are created only when their series exist. Unknown hardware
therefore shows fewer panels instead of errors.

| Panel | Content |
|---|---|
| Processor | Usage (hero figure), clocks, package temperature and power, I/O wait; stacked chart of user/system/I/O wait/other; a bar per thread, grouped by physical core (SMT siblings side by side), eight cores per row and at most three rows; hover a bar for its clock speed. |
| Load and pressure | Load averages, runnable and blocked tasks; PSI chart (falls back to a load chart without PSI). |
| Memory | Used, available, cache; stacked used/cache chart up to total memory; swap meter; paging rates. |
| GPU (one per GPU) | Load, temperature, power or SoC package power, clocks, video engine, voltages; video and shared memory meters; load chart (with encoder/decoder or video engine). A badge shows when a dGPU is sleeping. NVIDIA cards also show encoder load, sessions and FPS, decoder load, video clock, PCIe traffic, power against the limit, hardware facts (CUDA cores, architecture, PCIe link, clock limiters, driver and CUDA versions) and the processes using the GPU with their memory, SM, encoder and decoder use. |
| Temperatures | Chart of CPU, GPU, iGPU and SSD; the value of every sensor as a number, grouped by chip, with a status dot (and "warm", "hot" or "critical" when relevant). Single-sensor chips such as DIMMs share an "Other sensors" group. A °C/°F switch in the card header changes the unit everywhere. |
| Storage | Per disk: throughput, busy, latency and a read/write chart; filesystem usage meters. |
| Network | Per active interface: traffic, Wi-Fi signal and link quality, chart; socket counts; interfaces without link listed. |
| Power and battery | Measured total, CPU package, CPU cores, GPUs, memory; stacked power chart; battery meter and figures; AC and USB-C state. |
| Fans | RPM per fan and chart. |
| System | Uptime, processes, threads, open files; power profile, governor, energy preference, frequency driver, boost, firmware. |
| Processes | Process, running, blocked and zombie counts; the top 15 applications (processes grouped by program) by CPU or by memory, with process count, users, CPU, memory, GPU memory and threads. Selecting an application opens the process overlay; a single-process application opens its process view directly. |

## Colors and thresholds

- Series colors come from a colorblind-validated categorical palette (blue,
  orange, aqua, yellow), separate steps for light and dark themes.
- Status colors:

  | Status | Color |
  |---|---|
  | normal | blue |
  | warm, or 70–90 % | yellow |
  | hot | orange |
  | critical, or ≥ 90 % | red |

  They are always paired with a number or a word.
- **Per-thread bars:** blue under 70 %, yellow from 70 %, red from 90 %
  (`CORE_WARN` and `CORE_CRIT` in `cards.js`).
- **Temperature status:** the status dot depends on the sensor's critical
  value (100 °C when unknown).

  | Status | Range |
  |---|---|
  | warm | within 20 °C of critical |
  | hot | within 8 °C |
  | critical | within 3 °C |
- **Usage meters (memory, filesystems):** yellow from 80 %, red from 90 %.

## Smoothness

- **Numbers.** Every figure and bar eases toward each new value with an
  exponential approach. The time constant depends on `display.smoothing`:

  | Setting | Time constant |
  |---|---|
  | off | 0 ms |
  | low | 160 ms |
  | medium | 420 ms |
  | high | 900 ms |

  `prefers-reduced-motion` disables easing.
- **Live charts.** These scroll continuously. The x window trails "now" by
  one sampling interval, so the newest point enters at the right edge instead
  of jumping. Charts redraw at 15, 30 or 60 frames per second for low,
  medium and high smoothing; with smoothing off they redraw only when data
  arrives, which uses the least power. Only visible charts are redrawn.
- **Scales.** The y scale eases toward a rounded maximum (binary steps for
  bytes). Lines use spline paths in live mode.
- **Gaps.** A gap longer than three sampling intervals (suspend, restart)
  is drawn as a break.

## Live data and history

1. On load the page reads `/api/settings`, `/api/meta` and `/api/snapshot`,
   builds the panels and backfills charts from `/api/live`. It then follows
   `/api/stream`.
2. A `hello` with a new instance id (service restarted) or a catalog change
   that adds or removes series triggers a rebuild (debounced, keeps the
   scroll position). Series that appear later, such as a GPU waking up or
   battery time while discharging, therefore get their readouts and charts.
3. The time range buttons (1 h, 6 h, 24 h, 7 d, 30 d) switch every chart to
   `/api/history`. These charts show bucket averages, with a min–max band on
   single-series charts, and refresh every minute. Figures and bars always
   show live values. **Live** returns to scrolling charts.

When the stream drops, a banner appears after 2.5 s and the browser
reconnects automatically. A banner also shows memory-only mode and start-up
notes (for example, a database restored from backup).

## Process overlay

One dialog with two views. A back link at the top returns to the previous
view (up to 20 steps).

**Application view.** Opens from an application in the Processes card and
refreshes every 3 s. It shows:

- totals: CPU, memory, threads, and GPU memory from the NVIDIA process list;
- for multi-process programs (browsers, Electron apps), a breakdown by
  process type with process count, CPU, memory and GPU memory, plus a hint
  about the browser's own task manager for per-tab usage;
- every process with PID, type, user, CPU, memory, GPU memory, threads,
  state, running time and command line. Selecting a row (click, Enter or
  Space) opens its process view.

**Process view.** Opens from a process row, from a single-process
application in the Processes card, from a row of the "Processes using the
GPU" table, or from the parent or a child of another process. It reads
`/api/process` every 2 s and shows:

- readouts: CPU, memory, swap, threads, open files (of the limit), disk read
  and write rates, and GPU memory when the NVIDIA list has the process;
- CPU and memory trend lines since the view was opened (the window grows
  from 1 to 5 minutes; hover shows the value at a time). CPU and disk rates
  are computed from the difference between two reads;
- identity: kernel name, parent (opens its view), start time, systemd unit,
  executable, working directory, session and process group, sandbox flags;
- scheduling: CPU time (user and system), CPU time of waited-for children,
  nice or real-time priority, policy, the CPU it last ran on, allowed CPUs,
  voluntary and forced context switches, OOM score;
- memory breakdown (resident anonymous, file-backed and shared, peak,
  swap, virtual size, data, stack, libraries, page tables, locked);
- disk I/O totals;
- the command line, one argument per line;
- child processes (each opens its view) and the 40 busiest threads with
  their current CPU, CPU time and last CPU.

The kernel shows the executable, working directory, open files and disk
I/O only to the process owner and root. For other users' processes these
readouts and the disk I/O section are left out, the two paths read
"hidden", and a note under the identity block says why.

Values update in place, so text in the command line stays selectable.
The view pins the process by PID and start time: when the process exits, or
its PID is reused, it stops refreshing and keeps the last values, dimmed.

## Settings overlay

The gear button opens a dialog with five tabs: Statistics (per-collector
on/off and interval, ignored network interfaces), History, Backups, Display
and Status. Every control saves immediately through `PUT /api/settings`. All
open pages receive the change through the `settings` event. See
[settings.md](settings.md).
