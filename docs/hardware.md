# Hardware inventory

The service keeps a list of the physical hardware it has found and stores it
in the database. The list is shown in Settings → Hardware, where a button
starts a rescan. Code: `src/inventory.rs`.

## How names are found

No model names are built into hwstats. Every name is read from the system
at run time:

| Hardware | Source |
|---|---|
| Computer | DMI: `/sys/class/dmi/id/{sys_vendor,product_name,board_name,bios_version}` |
| Processor | `model name` in `/proc/cpuinfo`; core and thread counts from the `cpu` collector |
| Memory | `MemTotal` in `/proc/meminfo` (usable memory) |
| NVIDIA GPUs | NVML (or `nvidia-smi`), with static facts such as CUDA cores, VBIOS and driver |
| AMD/Intel GPUs | `product_name` in sysfs, else the system `pci.ids` database (`/usr/share/misc/pci.ids`, `/usr/share/hwdata/pci.ids`) |
| Disks | `model` in sysfs plus the kernel device name |
| Network adapters | interface name and type; only interfaces with a `device` link in sysfs (physical) |
| Batteries, power supplies | `power_supply` class: name, type and model |
| Sensor chips | hwmon driver name. Common drivers get a readable label (CPU, NVMe, memory DIMM, embedded controller, motherboard, Wi-Fi); any other driver shows as `driver (device)`. |

The fixed tables map kernel driver names and PCI vendor ids to labels, with
generic fallbacks, so unknown hardware still gets a name.

## What is stored

The inventory is one JSON document in the `meta` table (`key = 'hardware'`):

```json
{"format": 1, "scanned_ms": 1794600000000,
 "items": [{"key": "gpu_nvidia:0000-01-00.0", "kind": "gpu", "name": "NVIDIA Example GPU",
            "details": {"driver": "570.00", "vbios": "1.0"},
            "first_seen": 1794600000000, "last_seen": 1794603600000, "present": true}]}
```

- `kind` is `machine`, `cpu`, `memory`, `gpu`, `disk`, `network`, `power`
  or `sensor`.
- `key` is `machine`, `cpu`, `memory`, or `<collector>:<group>` for devices.
  Device keys use the collectors' stable group ids, so they survive reboots.
- `details` holds static facts of the device. Values that change while
  running (state, link speed, throttle reasons) and serial numbers are not
  stored. Virtual network interfaces, cores, filesystems and pressure are
  not part of the inventory.

A document with another `format` is ignored and rebuilt.

## Updates

- **First detection.** About 15 seconds after start-up, when the
  collectors have run, an empty inventory is filled with everything found.
- **Every 60 seconds.** New devices are added (for example a USB disk) with
  their current name. Each entry is marked present when its series were
  updated in the last 2 minutes, otherwise absent; absent entries are kept
  with their last-seen time. Stored names and details are kept. The document
  is written when something changes, and at least hourly to update
  `last_seen`.

The dashboard's card titles always use the live names from the collectors.
The inventory is the stored record.

## Rescan

`POST /api/hardware/rescan` (the **Rescan hardware** button) does the
following:

1. Re-reads the host facts (computer, processor, memory, operating system)
   and clears the cached `pci.ids` lookups.
2. Rebuilds the device-discovering collectors: `disk`, `network`,
   `sensors`, `thermal`, `power`, `battery`, `gpu_nvidia` and `gpu_drm`.
   Each gets a fresh instance, so device lists, names and caches are found
   again, and ticks at once, without waiting for the 30 s re-probe of an
   unavailable source or for an error backoff. A tick already running keeps
   its old instance, which is dropped when the tick returns. Other
   collectors keep running unchanged.
3. Waits until every enabled rebuilt collector has completed two ticks
   (rates need two samples), or one tick that found no source. The wait is
   at most twice the longest interval plus one second, between 4 and 30
   seconds.
4. Replaces the stored list with what was detected: names and details are
   refreshed, and `first_seen` is kept for known devices. An entry that was
   not found is removed only if its collector completed the ticks of step 3;
   otherwise (collector disabled, stalled or slow) it stays, marked present
   or absent from its recent updates.

Only one rescan runs at a time (409 while running), and a new one can start
10 seconds after the last one finished (429 before). Progress and the
result are sent as `hardware` events; open pages redraw the Hardware tab,
update the header and rebuild the dashboard after a rescan.

A sleeping NVIDIA GPU (runtime power management) stays asleep: the rebuilt
collector checks the power state before it initialises NVML.

Typical reasons to rescan: a driver was installed or updated, a kernel
module was loaded (for example `drivetemp`), `pci.ids` was updated, or a
part was replaced.
