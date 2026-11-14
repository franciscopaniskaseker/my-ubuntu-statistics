//! ACPI/platform thermal zones (/sys/class/thermal) that are not already
//! exported through hwmon (e.g. `x86_pkg_temp`, `iwlwifi`, SoC zones).

use super::{Collector, Ctx};
use crate::model::{DescCache, Health, Out, SeriesDesc};
use crate::util::parse::slug;
use crate::util::sysfs::{Roots, list_dir, read_opt, read_opt_f64};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct Thermal {
    roots: Roots,
    descs: DescCache,
    zones: Vec<(String, PathBuf, Option<f64>)>,
    last_scan: Option<Instant>,
}

impl Thermal {
    pub fn new(roots: Roots) -> Self {
        Self {
            roots,
            descs: DescCache::default(),
            zones: Vec::new(),
            last_scan: None,
        }
    }

    fn scan(&mut self) {
        let hwmon_names: HashSet<String> = list_dir(&self.roots.sys("class/hwmon"))
            .into_iter()
            .filter_map(|(_, d)| read_opt(&d.join("name")))
            .collect();
        let mut count: HashMap<String, u32> = HashMap::new();
        self.zones.clear();
        for (name, dir) in list_dir(&self.roots.sys("class/thermal")) {
            if !name.starts_with("thermal_zone") {
                continue;
            }
            let Some(ty) = read_opt(&dir.join("type")) else {
                continue;
            };
            if hwmon_names.contains(&ty) {
                continue;
            }
            let n = count.entry(ty.clone()).or_insert(0);
            *n += 1;
            let id = if *n > 1 {
                format!("{}.{}", slug(&ty), n)
            } else {
                slug(&ty)
            };
            // Look for the "critical" trip point.
            let mut crit = None;
            for i in 0..16 {
                match read_opt(&dir.join(format!("trip_point_{i}_type"))) {
                    Some(t) if t == "critical" => {
                        crit = read_opt_f64(&dir.join(format!("trip_point_{i}_temp")))
                            .map(|v| v / 1000.0)
                            .filter(|v| (20.0..=200.0).contains(v));
                        break;
                    }
                    Some(_) => continue,
                    None => break,
                }
            }
            self.zones.push((id, dir.join("temp"), crit));
        }
    }
}

impl Collector for Thermal {
    fn collect(&mut self, ctx: &Ctx, out: &mut Out) -> anyhow::Result<()> {
        if self
            .last_scan
            .is_none_or(|t| ctx.now.duration_since(t) > Duration::from_secs(60))
        {
            self.scan();
            self.last_scan = Some(ctx.now);
        }
        let mut ok = 0;
        for (id, path, crit) in &self.zones {
            let Some(v) = read_opt_f64(path).map(|v| v / 1000.0) else {
                continue;
            };
            if !(-50.0..=200.0).contains(&v) {
                continue;
            }
            ok += 1;
            let key = format!("thermal.{id}");
            let d = self.descs.get(&key, || {
                SeriesDesc::new("thermal", key.clone(), "temp", id.clone(), "C")
                    .group("zones", "Thermal zones")
                    .limits(None, *crit)
            });
            out.push(&d, v);
        }
        out.health = if ok > 0 {
            Health::ok("/sys/class/thermal")
        } else {
            Health::unavailable("no thermal zones beyond those covered by hwmon")
        };
        Ok(())
    }
}
