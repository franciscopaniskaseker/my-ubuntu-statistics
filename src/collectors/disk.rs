//! Block device I/O (/proc/diskstats) with stable per-disk identity.

use super::{Collector, Ctx};
use crate::model::{Desc, DescCache, Health, Out, SeriesDesc};
use crate::util::counter::{TickClock, delta};
use crate::util::parse::slug;
use crate::util::sysfs::{Roots, read_opt};
use anyhow::Context;
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DiskCounters {
    pub reads: u64,
    pub sectors_read: u64,
    pub ms_read: u64,
    pub writes: u64,
    pub sectors_written: u64,
    pub ms_write: u64,
    pub in_flight: u64,
    pub io_ms: u64,
}

/// Parse /proc/diskstats. Tolerates 14, 18 and 20 field variants.
pub fn parse_diskstats(s: &str) -> Vec<(String, DiskCounters)> {
    s.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 14 {
                return None;
            }
            let n = |i: usize| f.get(i).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
            Some((
                f[2].to_string(),
                DiskCounters {
                    reads: n(3),
                    sectors_read: n(5),
                    ms_read: n(6),
                    writes: n(7),
                    sectors_written: n(9),
                    ms_write: n(10),
                    in_flight: n(11),
                    io_ms: n(12),
                },
            ))
        })
        .collect()
}

#[derive(Debug, Clone)]
struct DiskInfo {
    id: String,
    label: String,
    /// Stable hardware name (the model) without the kernel device name.
    model: Option<String>,
    virtual_dev: bool,
}

pub struct Disk {
    roots: Roots,
    descs: DescCache,
    clock: TickClock,
    prev: HashMap<String, DiskCounters>,
    info: HashMap<String, Option<DiskInfo>>,
    last_scan: Option<Instant>,
    sata_without_temps: bool,
}

const SKIP_PREFIXES: &[&str] = &["loop", "ram", "zram", "fd", "sr", "nbd"];

impl Disk {
    pub fn new(roots: Roots) -> Self {
        Self {
            roots,
            descs: DescCache::default(),
            clock: TickClock::default(),
            prev: HashMap::new(),
            info: HashMap::new(),
            last_scan: None,
            sata_without_temps: false,
        }
    }

    fn identify(&self, name: &str) -> Option<DiskInfo> {
        if SKIP_PREFIXES.iter().any(|p| name.starts_with(p)) {
            return None;
        }
        let sysname = name.replace('/', "!");
        let base = self.roots.sys(&format!("block/{sysname}"));
        if !base.exists() {
            // Partitions are not listed directly under /sys/block.
            return None;
        }
        if let Some(dm) = read_opt(&base.join("dm/name")) {
            return Some(DiskInfo {
                id: format!("dm_{}", slug(&dm)),
                label: format!("{dm} ({name})"),
                model: None,
                virtual_dev: true,
            });
        }
        if name.starts_with("md") {
            return Some(DiskInfo {
                id: slug(name),
                label: format!("RAID {name}"),
                model: None,
                virtual_dev: true,
            });
        }
        let model = read_opt(&base.join("device/model"));
        let serial = read_opt(&base.join("device/serial"));
        let wwid = read_opt(&base.join("wwid")).or_else(|| read_opt(&base.join("device/wwid")));
        let id = match (&model, &serial, &wwid) {
            (Some(m), Some(s), _) => {
                let tail: String = s
                    .chars()
                    .rev()
                    .take(6)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                format!("{}_{}", slug(m), slug(&tail))
            }
            (_, _, Some(w)) => {
                let w = slug(w);
                w.chars()
                    .rev()
                    .take(40)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect()
            }
            _ => slug(name),
        };
        let label = match &model {
            Some(m) => format!("{m} ({name})"),
            None => name.to_string(),
        };
        Some(DiskInfo {
            id,
            label,
            model,
            virtual_dev: false,
        })
    }

    fn desc(&mut self, info: &DiskInfo, metric: &str, label: &str, unit: &'static str) -> Desc {
        let key = format!("disk.{}.{metric}", info.id);
        let make = || {
            SeriesDesc::new("disk", key.clone(), metric, label, unit)
                .group(info.id.clone(), info.label.clone())
        };
        let d = self.descs.get(&key, make);
        if d.group_label != info.label {
            return self.descs.put(make());
        }
        d
    }
}

impl Collector for Disk {
    fn collect(&mut self, ctx: &Ctx, out: &mut Out) -> anyhow::Result<()> {
        let p = self.roots.proc("diskstats");
        let s = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        let stats = parse_diskstats(&s);

        let rescan = self
            .last_scan
            .is_none_or(|t| ctx.now.duration_since(t) > Duration::from_secs(30));
        if rescan {
            self.info.clear();
            self.last_scan = Some(ctx.now);
            let has_sata = stats.iter().any(|(n, _)| {
                n.starts_with("sd")
                    && !n.chars().last().is_some_and(|c| c.is_ascii_digit())
                    && self
                        .roots
                        .sys(&format!("block/{n}/device/scsi_disk"))
                        .exists()
            });
            let has_drivetemp = crate::util::sysfs::list_dir(&self.roots.sys("class/hwmon"))
                .iter()
                .any(|(_, d)| {
                    crate::util::sysfs::read_opt(&d.join("name")).as_deref() == Some("drivetemp")
                });
            self.sata_without_temps = has_sata && !has_drivetemp;
        }

        let dt = self.clock.tick(ctx.now, ctx.interval);
        let (mut tot_r, mut tot_w) = (0.0, 0.0);
        let mut any = false;
        let mut new_prev = HashMap::with_capacity(stats.len());
        for (name, cur) in stats {
            if !self.info.contains_key(&name) {
                let i = self.identify(&name);
                self.info.insert(name.clone(), i);
            }
            let Some(info) = self.info.get(&name).cloned().flatten() else {
                continue;
            };
            any = true;
            // Facts for the hardware inventory: a stable name, and a flag
            // for device-mapper and RAID devices (not hardware).
            if info.virtual_dev {
                out.info(format!("{}.virtual", info.id), "yes");
            } else if let Some(m) = &info.model {
                out.info(format!("{}.name", info.id), m.clone());
            }
            if let (Some(dt), Some(prev)) = (dt, self.prev.get(&name).copied()) {
                let reads = delta(prev.reads, cur.reads);
                let writes = delta(prev.writes, cur.writes);
                let sr = delta(prev.sectors_read, cur.sectors_read);
                let sw = delta(prev.sectors_written, cur.sectors_written);
                let io = delta(prev.io_ms, cur.io_ms);
                let tms = delta(prev.ms_read + prev.ms_write, cur.ms_read + cur.ms_write);
                if let (Some(reads), Some(writes), Some(sr), Some(sw), Some(io)) =
                    (reads, writes, sr, sw, io)
                {
                    let rbps = sr as f64 * 512.0 / dt;
                    let wbps = sw as f64 * 512.0 / dt;
                    let d = self.desc(&info, "read_bps", "Read", "B/s");
                    out.push(&d, rbps);
                    let d = self.desc(&info, "write_bps", "Write", "B/s");
                    out.push(&d, wbps);
                    let d = self.desc(&info, "read_iops", "Read IOPS", "/s");
                    out.push(&d, reads as f64 / dt);
                    let d = self.desc(&info, "write_iops", "Write IOPS", "/s");
                    out.push(&d, writes as f64 / dt);
                    let d = self.desc(&info, "util", "Busy", "%");
                    out.push(&d, (io as f64 / (dt * 1000.0) * 100.0).clamp(0.0, 100.0));
                    let ops = reads + writes;
                    let d = self.desc(&info, "await", "Latency", "ms");
                    out.push(
                        &d,
                        if ops > 0 {
                            tms.unwrap_or(0) as f64 / ops as f64
                        } else {
                            0.0
                        },
                    );
                    if !info.virtual_dev {
                        tot_r += rbps;
                        tot_w += wbps;
                    }
                }
            }
            let d = self.desc(&info, "inflight", "In flight", "count");
            out.push(&d, cur.in_flight as f64);
            new_prev.insert(name, cur);
        }
        self.prev = new_prev;
        if dt.is_some() && any {
            let d = self.descs.get("disk.total.read_bps", || {
                SeriesDesc::new(
                    "disk",
                    "disk.total.read_bps",
                    "total_read_bps",
                    "Total read",
                    "B/s",
                )
            });
            out.push(&d, tot_r);
            let d = self.descs.get("disk.total.write_bps", || {
                SeriesDesc::new(
                    "disk",
                    "disk.total.write_bps",
                    "total_write_bps",
                    "Total write",
                    "B/s",
                )
            });
            out.push(&d, tot_w);
        }
        out.health = if any {
            let h = Health::ok("/proc/diskstats");
            if self.sata_without_temps {
                h.hint("SATA disk temperatures appear after loading the drivetemp kernel module (sudo modprobe drivetemp)")
            } else {
                h
            }
        } else {
            Health::unavailable("no block devices found")
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_variants() {
        let s = "   8       0 sda 100 0 2000 50 200 0 4000 70 1 120 130\n\
                 259 0 nvme0n1 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17\n\
                 259 1 nvme0n1p1 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20\n\
                 1 2 short 1 2\n";
        let v = parse_diskstats(s);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].1.sectors_read, 2000);
        assert_eq!(v[0].1.io_ms, 120);
        assert_eq!(v[1].1.in_flight, 9);
    }

    #[test]
    fn fixture_identity_is_stable() {
        let roots = Roots::fixture("laptop");
        let d = Disk::new(roots);
        let i = d.identify("nvme1n1").unwrap();
        assert_eq!(i.id, "example_ssd_2tb_123456");
        assert!(d.identify("nvme1n1p1").is_none());
        assert!(d.identify("loop0").is_none());
    }
}
