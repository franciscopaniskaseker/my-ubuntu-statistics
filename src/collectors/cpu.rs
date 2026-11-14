//! CPU usage (/proc/stat) and frequency (cpufreq, fallback /proc/cpuinfo).

use super::{Collector, Ctx};
use crate::model::{DescCache, Health, Out, SeriesDesc};
use crate::util::counter::{TickClock, delta};
use crate::util::sysfs::{Roots, read_opt_u64};
use anyhow::Context;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Times {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

impl Times {
    fn total(&self) -> u64 {
        self.user
            .saturating_add(self.nice)
            .saturating_add(self.system)
            .saturating_add(self.idle)
            .saturating_add(self.iowait)
            .saturating_add(self.irq)
            .saturating_add(self.softirq)
            .saturating_add(self.steal)
    }
}

#[derive(Debug, Default, Clone)]
pub struct Stat {
    pub total: Option<Times>,
    pub cores: Vec<(usize, Times)>,
    pub ctxt: Option<u64>,
    pub intr: Option<u64>,
    pub procs_running: Option<u64>,
    pub procs_blocked: Option<u64>,
}

fn parse_times(rest: &str) -> Times {
    let mut v = rest
        .split_whitespace()
        .map(|x| x.parse::<u64>().unwrap_or(0));
    let mut n = || v.next().unwrap_or(0);
    // guest/guest_nice are already included in user/nice.
    Times {
        user: n(),
        nice: n(),
        system: n(),
        idle: n(),
        iowait: n(),
        irq: n(),
        softirq: n(),
        steal: n(),
    }
}

pub fn parse_stat(s: &str) -> Stat {
    let mut st = Stat::default();
    for line in s.lines() {
        let Some((head, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if head == "cpu" {
            st.total = Some(parse_times(rest));
        } else if let Some(n) = head.strip_prefix("cpu") {
            if let Ok(idx) = n.parse::<usize>() {
                st.cores.push((idx, parse_times(rest)));
            }
        } else {
            let first = rest
                .split_whitespace()
                .next()
                .and_then(|x| x.parse::<u64>().ok());
            match head {
                "ctxt" => st.ctxt = first,
                "intr" => st.intr = first,
                "procs_running" => st.procs_running = first,
                "procs_blocked" => st.procs_blocked = first,
                _ => {}
            }
        }
    }
    st
}

/// MHz per logical CPU from /proc/cpuinfo (order of `processor` entries).
pub fn parse_cpuinfo_mhz(s: &str) -> Vec<(usize, f64)> {
    let mut out = Vec::new();
    let mut cur: Option<usize> = None;
    for line in s.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        match k.trim() {
            "processor" => cur = v.trim().parse().ok(),
            "cpu MHz" => {
                if let (Some(p), Ok(m)) = (cur, v.trim().parse::<f64>()) {
                    out.push((p, m));
                }
            }
            _ => {}
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FreqSource {
    Unknown,
    Cpufreq,
    Cpuinfo,
    None,
}

pub struct Cpu {
    roots: Roots,
    prev: Option<Stat>,
    clock: TickClock,
    descs: DescCache,
    freq: FreqSource,
    /// Logical CPU -> (package, physical core) from sysfs topology.
    topo: std::collections::HashMap<usize, (u32, u32)>,
    multi_pkg: bool,
    topo_for: usize,
}

impl Cpu {
    pub fn new(roots: Roots) -> Self {
        Self {
            roots,
            prev: None,
            clock: TickClock::default(),
            descs: DescCache::default(),
            freq: FreqSource::Unknown,
            topo: std::collections::HashMap::new(),
            multi_pkg: false,
            topo_for: 0,
        }
    }

    /// Read the physical core of every logical CPU (re-read when the number
    /// of CPUs changes, e.g. after hotplug).
    fn load_topology(&mut self, cores: &[usize]) {
        if self.topo_for == cores.len() {
            return;
        }
        self.topo_for = cores.len();
        self.topo.clear();
        for &c in cores {
            let base = self
                .roots
                .sys(&format!("devices/system/cpu/cpu{c}/topology"));
            let core = read_opt_u64(&base.join("core_id"));
            let pkg = read_opt_u64(&base.join("physical_package_id")).unwrap_or(0);
            if let Some(core) = core {
                self.topo.insert(c, (pkg as u32, core as u32));
            }
        }
        let pkgs: std::collections::HashSet<u32> = self.topo.values().map(|(p, _)| *p).collect();
        self.multi_pkg = pkgs.len() > 1;
        self.descs = DescCache::default();
    }

    /// Group (physical core) of a logical CPU for the descriptors.
    fn core_group(&self, idx: usize) -> (String, String) {
        match self.topo.get(&idx) {
            Some((p, c)) if self.multi_pkg => {
                (format!("pkg{p}.core{c}"), format!("core {c}, package {p}"))
            }
            Some((_, c)) => (format!("core{c}"), format!("core {c}")),
            None => (format!("cpu{idx}"), format!("CPU {idx}")),
        }
    }

    fn d(
        &mut self,
        key: &str,
        metric: &str,
        label: &str,
        unit: &'static str,
    ) -> crate::model::Desc {
        self.descs
            .get(key, || SeriesDesc::new("cpu", key, metric, label, unit))
    }

    fn core_desc(&mut self, idx: usize, metric: &str, unit: &'static str) -> crate::model::Desc {
        let key = format!("cpu.core.{idx}.{metric}");
        let (gid, glabel) = self.core_group(idx);
        self.descs.get(&key, || {
            SeriesDesc::new(
                "cpu",
                key.clone(),
                format!("core_{metric}"),
                format!("cpu{idx}"),
                unit,
            )
            .group(gid, glabel)
        })
    }

    fn read_freqs(&mut self, cores: &[usize]) -> Vec<(usize, f64)> {
        if self.freq == FreqSource::Unknown || self.freq == FreqSource::Cpufreq {
            let mut v = Vec::with_capacity(cores.len());
            for &c in cores {
                let p = self.roots.sys(&format!(
                    "devices/system/cpu/cpu{c}/cpufreq/scaling_cur_freq"
                ));
                if let Some(khz) = read_opt_u64(&p) {
                    v.push((c, khz as f64 / 1000.0));
                }
            }
            if !v.is_empty() {
                self.freq = FreqSource::Cpufreq;
                return v;
            }
        }
        if let Ok(s) = std::fs::read_to_string(self.roots.proc("cpuinfo")) {
            let v = parse_cpuinfo_mhz(&s);
            if !v.is_empty() {
                self.freq = FreqSource::Cpuinfo;
                return v;
            }
        }
        self.freq = FreqSource::None;
        Vec::new()
    }
}

fn pct(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (part as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    }
}

impl Collector for Cpu {
    fn collect(&mut self, ctx: &Ctx, out: &mut Out) -> anyhow::Result<()> {
        let path = self.roots.proc("stat");
        let s = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cur = parse_stat(&s);
        if cur.total.is_none() {
            anyhow::bail!("no cpu line in {}", path.display());
        }
        let dt = self.clock.tick(ctx.now, ctx.interval);
        let indices: Vec<usize> = cur.cores.iter().map(|(i, _)| *i).collect();
        self.load_topology(&indices);

        let prev_snap = self.prev.take();
        if let (Some(prev), Some(dt)) = (&prev_snap, dt) {
            if let (Some(p), Some(c)) = (prev.total, cur.total)
                && let Some(dtot) = delta(p.total(), c.total()).filter(|d| *d > 0)
            {
                let f = |a: u64, b: u64| delta(a, b).unwrap_or(0);
                let idle = f(p.idle, c.idle) + f(p.iowait, c.iowait);
                let parts = [
                    ("cpu.user", "user", "User", f(p.user, c.user)),
                    ("cpu.nice", "nice", "Nice", f(p.nice, c.nice)),
                    ("cpu.system", "system", "System", f(p.system, c.system)),
                    ("cpu.iowait", "iowait", "I/O wait", f(p.iowait, c.iowait)),
                    ("cpu.irq", "irq", "IRQ", f(p.irq, c.irq)),
                    ("cpu.softirq", "softirq", "SoftIRQ", f(p.softirq, c.softirq)),
                    ("cpu.steal", "steal", "Steal", f(p.steal, c.steal)),
                ];
                let d = self.d("cpu.usage", "usage", "CPU usage", "%");
                out.push(&d, pct(dtot.saturating_sub(idle), dtot));
                for (key, metric, label, v) in parts {
                    let d = self.d(key, metric, label, "%");
                    out.push(&d, pct(v, dtot));
                }
            }
            let prev_cores: std::collections::HashMap<usize, Times> =
                prev.cores.iter().copied().collect();
            for (idx, c) in &cur.cores {
                if let Some(p) = prev_cores.get(idx)
                    && let Some(dtot) = delta(p.total(), c.total()).filter(|d| *d > 0)
                {
                    let idle =
                        delta(p.idle, c.idle).unwrap_or(0) + delta(p.iowait, c.iowait).unwrap_or(0);
                    let d = self.core_desc(*idx, "usage", "%");
                    out.push(&d, pct(dtot.saturating_sub(idle), dtot));
                }
            }
            if let (Some(a), Some(b)) = (prev.ctxt, cur.ctxt)
                && let Some(x) = delta(a, b)
            {
                let d = self.d("cpu.ctxt", "ctxt", "Context switches", "/s");
                out.push(&d, x as f64 / dt);
            }
            if let (Some(a), Some(b)) = (prev.intr, cur.intr)
                && let Some(x) = delta(a, b)
            {
                let d = self.d("cpu.intr", "intr", "Interrupts", "/s");
                out.push(&d, x as f64 / dt);
            }
        }
        if let Some(v) = cur.procs_running {
            let d = self.d(
                "cpu.procs_running",
                "procs_running",
                "Runnable tasks",
                "count",
            );
            out.push(&d, v as f64);
        }
        if let Some(v) = cur.procs_blocked {
            let d = self.d(
                "cpu.procs_blocked",
                "procs_blocked",
                "Blocked on I/O",
                "count",
            );
            out.push(&d, v as f64);
        }

        let cores: Vec<usize> = cur.cores.iter().map(|(i, _)| *i).collect();
        let freqs = self.read_freqs(&cores);
        if !freqs.is_empty() {
            let mut sum = 0.0;
            let mut max: f64 = 0.0;
            for (idx, mhz) in &freqs {
                let d = self.core_desc(*idx, "mhz", "MHz");
                out.push(&d, *mhz);
                sum += mhz;
                max = max.max(*mhz);
            }
            let d = self.d("cpu.mhz_avg", "mhz_avg", "Average frequency", "MHz");
            out.push(&d, sum / freqs.len() as f64);
            let d = self.d("cpu.mhz_max", "mhz_max", "Highest core frequency", "MHz");
            out.push(&d, max);
        }

        out.info("threads", cur.cores.len().to_string());
        let physical: std::collections::HashSet<(u32, u32)> = self.topo.values().copied().collect();
        if !physical.is_empty() {
            out.info("cores", physical.len().to_string());
        }
        out.health = match self.freq {
            FreqSource::Cpufreq => Health::ok("/proc/stat, cpufreq"),
            FreqSource::Cpuinfo => Health::degraded(
                "/proc/stat, /proc/cpuinfo",
                "cpufreq not available; frequency from /proc/cpuinfo",
            ),
            _ => Health::degraded("/proc/stat", "CPU frequency not available"),
        };
        self.prev = Some(cur);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat() {
        let s = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 50 0 25 400 25 0 0 0 0 0\ncpu1 50 0 25 400 25\nintr 12345 1 2 3\nctxt 999\nprocs_running 3\nprocs_blocked 1\n";
        let st = parse_stat(s);
        assert_eq!(st.total.unwrap().user, 100);
        assert_eq!(st.cores.len(), 2);
        assert_eq!(st.cores[1].1.iowait, 25);
        assert_eq!(st.cores[1].1.steal, 0);
        assert_eq!(st.intr, Some(12345));
        assert_eq!(st.ctxt, Some(999));
        assert_eq!(st.procs_running, Some(3));
    }

    #[test]
    fn parses_cpuinfo() {
        let s = "processor\t: 0\ncpu MHz\t\t: 3000.123\n\nprocessor\t: 1\ncpu MHz\t\t: 1500\n";
        assert_eq!(parse_cpuinfo_mhz(s), vec![(0, 3000.123), (1, 1500.0)]);
    }

    #[test]
    fn fixture_vm_uses_cpuinfo_fallback() {
        let roots = Roots::fixture("vm");
        let mut c = Cpu::new(roots);
        let settings = crate::settings::Settings::default();
        let now = std::time::Instant::now();
        let ctx = Ctx {
            now,
            wall_ms: 0,
            interval: std::time::Duration::from_secs(1),
            settings: &settings,
        };
        let mut out = Out::new();
        c.collect(&ctx, &mut out).unwrap();
        assert_eq!(out.health.state, crate::model::State::Degraded);
        assert!(out.points.iter().any(|(d, _)| d.key == "cpu.core.0.mhz"));
    }
}
