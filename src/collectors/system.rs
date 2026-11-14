//! Load average, uptime, open files and platform facts (power profile,
//! cpufreq governor/EPP).

use super::{Collector, Ctx};
use crate::model::{DescCache, Health, Out, SeriesDesc};
use crate::util::sysfs::{Roots, read_opt};
use anyhow::Context;

pub struct System {
    roots: Roots,
    descs: DescCache,
}

impl System {
    pub fn new(roots: Roots) -> Self {
        Self {
            roots,
            descs: DescCache::default(),
        }
    }

    fn push(
        &mut self,
        out: &mut Out,
        key: &str,
        metric: &str,
        label: &str,
        unit: &'static str,
        v: f64,
    ) {
        let d = self
            .descs
            .get(key, || SeriesDesc::new("system", key, metric, label, unit));
        out.push(&d, v);
    }
}

/// Parse /proc/loadavg: "0.52 0.58 0.59 2/1234 5678".
/// (load1, load5, load15, runnable, total threads)
pub type LoadAvg = (f64, f64, f64, Option<u64>, Option<u64>);

pub fn parse_loadavg(s: &str) -> Option<LoadAvg> {
    let mut it = s.split_whitespace();
    let l1 = it.next()?.parse().ok()?;
    let l5 = it.next()?.parse().ok()?;
    let l15 = it.next()?.parse().ok()?;
    let (run, total) = match it.next().and_then(|x| x.split_once('/')) {
        Some((a, b)) => (a.parse().ok(), b.parse().ok()),
        None => (None, None),
    };
    Some((l1, l5, l15, run, total))
}

impl Collector for System {
    fn collect(&mut self, _ctx: &Ctx, out: &mut Out) -> anyhow::Result<()> {
        let p = self.roots.proc("loadavg");
        let s = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        let (l1, l5, l15, _run, threads) = parse_loadavg(&s)
            .ok_or_else(|| anyhow::anyhow!("unexpected format in {}", p.display()))?;
        self.push(out, "load.1", "load1", "Load 1 min", "count", l1);
        self.push(out, "load.5", "load5", "Load 5 min", "count", l5);
        self.push(out, "load.15", "load15", "Load 15 min", "count", l15);
        if let Some(t) = threads {
            self.push(out, "sys.threads", "threads", "Threads", "count", t as f64);
        }

        if let Some(up) = read_opt(&self.roots.proc("uptime")).and_then(|s| {
            s.split_whitespace()
                .next()
                .and_then(|x| x.parse::<f64>().ok())
        }) {
            self.push(out, "sys.uptime", "uptime", "Uptime", "s", up);
        }
        if let Some(fds) = read_opt(&self.roots.proc("sys/fs/file-nr")).and_then(|s| {
            s.split_whitespace()
                .next()
                .and_then(|x| x.parse::<f64>().ok())
        }) {
            self.push(
                out,
                "sys.open_files",
                "open_files",
                "Open files",
                "count",
                fds,
            );
        }

        let cpu0 = self.roots.sys("devices/system/cpu/cpu0/cpufreq");
        if let Some(v) = read_opt(&self.roots.sys("firmware/acpi/platform_profile")) {
            out.info("platform_profile", v);
        }
        if let Some(v) = read_opt(&cpu0.join("scaling_governor")) {
            out.info("governor", v);
        }
        if let Some(v) = read_opt(&cpu0.join("scaling_driver")) {
            out.info("cpufreq_driver", v);
        }
        if let Some(v) = read_opt(&cpu0.join("energy_performance_preference")) {
            out.info("epp", v);
        }
        if let Some(v) =
            read_opt(&self.roots.sys("devices/system/cpu/cpufreq/boost")).or_else(|| {
                read_opt(&self.roots.sys("devices/system/cpu/intel_pstate/no_turbo"))
                    .map(|x| if x == "1" { "0".into() } else { "1".into() })
            })
        {
            out.info("boost", if v == "1" { "on" } else { "off" });
        }
        out.health = Health::ok("/proc/loadavg, /proc/uptime, sysfs");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loadavg() {
        let (a, b, c, r, t) = parse_loadavg("0.52 0.58 0.59 2/1234 5678\n").unwrap();
        assert_eq!((a, b, c, r, t), (0.52, 0.58, 0.59, Some(2), Some(1234)));
        assert!(parse_loadavg("garbage").is_none());
    }
}
