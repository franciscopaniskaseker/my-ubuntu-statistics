//! CPU/platform power from RAPL energy counters (powercap), with a fallback
//! to hwmon power/energy sensors (amd_energy, zenpower, fam15h_power).

use super::hwmon;
use super::{Collector, Ctx};
use crate::model::{DescCache, Health, Out, SeriesDesc};
use crate::util::counter::Rates;
use crate::util::parse::slug;
use crate::util::sysfs::{Roots, list_dir, read_opt, read_opt_u64, read_u64};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct Zone {
    key: String,
    label: String,
    energy: PathBuf,
    max_range: Option<u64>,
}

const FALLBACK_CHIPS: &[&str] = &["zenpower", "amd_energy", "fam15h_power"];

pub struct Power {
    roots: Roots,
    descs: DescCache,
    rates: Rates,
    zones: Vec<Zone>,
    last_scan: Option<Instant>,
    fallback_chips: Vec<hwmon::Chip>,
    fallback_scan: Option<Instant>,
}

impl Power {
    pub fn new(roots: Roots) -> Self {
        Self {
            roots,
            descs: DescCache::default(),
            rates: Rates::default(),
            zones: Vec::new(),
            last_scan: None,
            fallback_chips: Vec::new(),
            fallback_scan: None,
        }
    }

    fn scan(&mut self) {
        self.zones.clear();
        let base = self.roots.sys("class/powercap");
        let entries = list_dir(&base);
        for (name, dir) in &entries {
            if !name.contains("rapl") || !dir.join("energy_uj").exists() {
                continue;
            }
            let zname = read_opt(&dir.join("name")).unwrap_or_else(|| name.clone());
            // intel-rapl:0:1 -> parent intel-rapl:0
            let parent_name = name
                .rsplit_once(':')
                .filter(|(p, _)| p.contains(':'))
                .and_then(|(p, _)| read_opt(&base.join(p).join("name")));
            let (key, label) = match &parent_name {
                Some(p) => (
                    format!("power.rapl.{}.{}", slug(p), slug(&zname)),
                    pretty(&zname, Some(p)),
                ),
                None => (format!("power.rapl.{}", slug(&zname)), pretty(&zname, None)),
            };
            if self.zones.iter().any(|z| z.key == key) {
                continue;
            }
            self.zones.push(Zone {
                key,
                label,
                energy: dir.join("energy_uj"),
                max_range: read_opt_u64(&dir.join("max_energy_range_uj")),
            });
        }
    }

    fn fallback(&mut self, out: &mut Out, ctx: &Ctx) -> usize {
        let mut n = 0;
        if self
            .fallback_scan
            .is_none_or(|t| ctx.now.duration_since(t) > Duration::from_secs(300))
        {
            self.fallback_chips = hwmon::scan(&self.roots)
                .into_iter()
                .filter(|c| FALLBACK_CHIPS.contains(&c.name.as_str()))
                .collect();
            self.fallback_scan = Some(ctx.now);
        }
        for chip in self.fallback_chips.clone() {
            for s in &chip.sensors {
                if s.kind != "power" {
                    continue;
                }
                if let Ok(Some(v)) = hwmon::read_sensor(s) {
                    let key = format!("power.hwmon.{}.{}{}", slug(&chip.name), s.kind, s.index);
                    let d = self.descs.get(&key, || {
                        SeriesDesc::new(
                            "power",
                            key.clone(),
                            "power",
                            format!("{} {}", chip.name, s.label),
                            "W",
                        )
                        .group("cpu", "CPU power")
                    });
                    out.push(&d, v);
                    n += 1;
                }
            }
            // amd_energy exposes energy counters (µJ) instead of power.
            for (fname, path) in list_dir(
                chip.dev_path
                    .as_deref()
                    .unwrap_or(std::path::Path::new("/nonexistent")),
            ) {
                if !(fname.starts_with("energy") && fname.ends_with("_input")) {
                    continue;
                }
                let label = read_opt(&path.with_file_name(fname.replace("_input", "_label")))
                    .unwrap_or(fname.clone());
                if !label.starts_with("Esocket") {
                    continue;
                }
                if let Some(e) = read_opt_u64(&path) {
                    let key = format!("power.hwmon.amd_energy.{}", slug(&label));
                    if let Some(w) = self.rates.rate(&key, e, ctx.now, ctx.interval) {
                        let d = self.descs.get(&key, || {
                            SeriesDesc::new(
                                "power",
                                key.clone(),
                                "power",
                                format!("CPU {label}"),
                                "W",
                            )
                            .group("cpu", "CPU power")
                        });
                        out.push(&d, w / 1_000_000.0);
                    }
                    n += 1;
                }
            }
        }
        n
    }
}

fn pretty(zone: &str, parent: Option<&str>) -> String {
    let base = match zone {
        z if z.starts_with("package") => {
            format!("CPU package {}", z.trim_start_matches("package-"))
        }
        "core" => "CPU cores".into(),
        "uncore" => "Uncore / iGPU".into(),
        "dram" => "Memory (DRAM)".into(),
        "psys" => "Platform (psys)".into(),
        z => z.to_string(),
    };
    match parent {
        Some(p) if p.starts_with("package") && p != "package-0" => format!("{base} ({p})"),
        _ => base,
    }
}

impl Collector for Power {
    fn collect(&mut self, ctx: &Ctx, out: &mut Out) -> anyhow::Result<()> {
        if self
            .last_scan
            .is_none_or(|t| ctx.now.duration_since(t) > Duration::from_secs(300))
        {
            self.scan();
            self.last_scan = Some(ctx.now);
        }
        let mut ok = 0;
        let mut denied = false;
        let mut other_err: Option<String> = None;
        for z in self.zones.clone() {
            match read_u64(&z.energy) {
                Ok(e) => {
                    ok += 1;
                    if let Some(rate) =
                        self.rates
                            .rate_wrap(&z.key, e, ctx.now, ctx.interval, z.max_range)
                    {
                        let d = self.descs.get(&z.key, || {
                            SeriesDesc::new("power", z.key.clone(), "power", z.label.clone(), "W")
                                .group("rapl", "RAPL")
                        });
                        let w = rate / 1_000_000.0;
                        if (0.0..=5000.0).contains(&w) {
                            out.push(&d, w);
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => denied = true,
                Err(e) => other_err = Some(e.to_string()),
            }
        }
        if ok > 0 {
            out.health = Health::ok("powercap RAPL");
            return Ok(());
        }
        let fb = self.fallback(out, ctx);
        let why = if denied {
            "RAPL energy counters are root-only (needs CAP_DAC_READ_SEARCH)".to_string()
        } else if let Some(e) = other_err {
            format!("RAPL read failed: {e}")
        } else {
            "no RAPL powercap zones".to_string()
        };
        out.health = if fb > 0 {
            Health::degraded("hwmon power sensors", why)
        } else {
            let h = Health::unavailable(why);
            if denied {
                h.hint("run as the hwstats service (it has CAP_DAC_READ_SEARCH) or as root")
            } else {
                h.hint("CPU power needs the intel_rapl / amd RAPL powercap driver or zenpower")
            }
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rapl_wrap_from_fixture() {
        let roots = Roots::fixture("laptop");
        let mut p = Power::new(roots);
        p.scan();
        let keys: Vec<&str> = p.zones.iter().map(|z| z.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["power.rapl.package-0", "power.rapl.package-0.core"]
        );
        assert_eq!(p.zones[0].max_range, Some(65532610987));
        assert_eq!(p.zones[0].label, "CPU package 0");
    }
}
