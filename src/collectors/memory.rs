//! Memory and swap (/proc/meminfo) plus paging activity (/proc/vmstat).

use super::{Collector, Ctx};
use crate::model::{DescCache, Health, Out, SeriesDesc};
use crate::util::counter::Rates;
use crate::util::parse::{key_number_lines, page_size, space_number_lines};
use crate::util::sysfs::Roots;
use anyhow::Context;
use std::collections::HashMap;

pub struct Memory {
    roots: Roots,
    descs: DescCache,
    rates: Rates,
    page: u64,
}

impl Memory {
    pub fn new(roots: Roots) -> Self {
        Self {
            roots,
            descs: DescCache::default(),
            rates: Rates::default(),
            page: page_size(),
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
            .get(key, || SeriesDesc::new("memory", key, metric, label, unit));
        out.push(&d, v);
    }
}

impl Collector for Memory {
    fn collect(&mut self, ctx: &Ctx, out: &mut Out) -> anyhow::Result<()> {
        let p = self.roots.proc("meminfo");
        let s = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        let m: HashMap<&str, u64> = key_number_lines(&s).map(|(k, v)| (k, v * 1024)).collect();
        let total = *m
            .get("MemTotal")
            .ok_or_else(|| anyhow::anyhow!("MemTotal missing in {}", p.display()))?;
        let free = m.get("MemFree").copied().unwrap_or(0);
        let buffers = m.get("Buffers").copied().unwrap_or(0);
        let cached = m.get("Cached").copied().unwrap_or(0);
        let sreclaim = m.get("SReclaimable").copied().unwrap_or(0);
        let mut degraded = None;
        let available = match m.get("MemAvailable") {
            Some(v) => *v,
            None => {
                degraded = Some("MemAvailable missing (old kernel); estimated");
                free + buffers + cached + sreclaim
            }
        }
        .min(total);
        let used = total.saturating_sub(available);

        self.push(out, "mem.total", "total", "Total", "B", total as f64);
        self.push(out, "mem.used", "used", "Used", "B", used as f64);
        self.push(
            out,
            "mem.available",
            "available",
            "Available",
            "B",
            available as f64,
        );
        self.push(out, "mem.free", "free", "Free", "B", free as f64);
        self.push(
            out,
            "mem.cached",
            "cached",
            "Cache",
            "B",
            (cached + sreclaim + buffers) as f64,
        );
        if total > 0 {
            self.push(
                out,
                "mem.used_pct",
                "used_pct",
                "Used",
                "%",
                used as f64 / total as f64 * 100.0,
            );
        }
        for (k, key, metric, label) in [
            ("Shmem", "mem.shmem", "shmem", "Shared"),
            ("Dirty", "mem.dirty", "dirty", "Dirty"),
            ("Writeback", "mem.writeback", "writeback", "Writeback"),
            ("AnonPages", "mem.anon", "anon", "Anonymous"),
            ("Slab", "mem.slab", "slab", "Slab"),
        ] {
            if let Some(v) = m.get(k) {
                self.push(out, key, metric, label, "B", *v as f64);
            }
        }

        let swap_total = m.get("SwapTotal").copied().unwrap_or(0);
        let swap_free = m.get("SwapFree").copied().unwrap_or(0);
        self.push(
            out,
            "swap.total",
            "swap_total",
            "Swap total",
            "B",
            swap_total as f64,
        );
        self.push(
            out,
            "swap.used",
            "swap_used",
            "Swap used",
            "B",
            swap_total.saturating_sub(swap_free) as f64,
        );
        if swap_total > 0 {
            self.push(
                out,
                "swap.used_pct",
                "swap_used_pct",
                "Swap used",
                "%",
                swap_total.saturating_sub(swap_free) as f64 / swap_total as f64 * 100.0,
            );
        }

        if let Ok(vs) = std::fs::read_to_string(self.roots.proc("vmstat")) {
            let v: HashMap<&str, u64> = space_number_lines(&vs).collect();
            let page = self.page as f64;
            for (k, key, metric, label, unit, scale) in [
                ("pswpin", "swap.in", "swap_in", "Swap in", "B/s", page),
                ("pswpout", "swap.out", "swap_out", "Swap out", "B/s", page),
                (
                    "pgmajfault",
                    "mem.majfault",
                    "majfault",
                    "Major page faults",
                    "/s",
                    1.0,
                ),
                ("pgpgin", "mem.pagein", "pagein", "Paged in", "B/s", 1024.0),
                (
                    "pgpgout",
                    "mem.pageout",
                    "pageout",
                    "Paged out",
                    "B/s",
                    1024.0,
                ),
            ] {
                if let Some(val) = v.get(k)
                    && let Some(r) = self.rates.rate(k, *val, ctx.now, ctx.interval)
                {
                    self.push(out, key, metric, label, unit, r * scale);
                }
            }
        }

        out.health = match degraded {
            Some(r) => Health::degraded("/proc/meminfo", r),
            None => Health::ok("/proc/meminfo, /proc/vmstat"),
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_kernel_estimates_available() {
        let roots = Roots::fixture("rhel8");
        let mut m = Memory::new(roots);
        let settings = crate::settings::Settings::default();
        let ctx = Ctx {
            now: std::time::Instant::now(),
            wall_ms: 0,
            interval: std::time::Duration::from_secs(1),
            settings: &settings,
        };
        let mut out = Out::new();
        m.collect(&ctx, &mut out).unwrap();
        assert_eq!(out.health.state, crate::model::State::Degraded);
        let used = out
            .points
            .iter()
            .find(|(d, _)| d.key == "mem.used")
            .unwrap()
            .1;
        assert!(used > 0.0);
    }
}
