//! Derived series computed from other collectors' latest values.
//!
//! Power summary (`power.cpu_total`, `power.gpu_total`, `power.dram_total`,
//! `power.total`): the measured total is the sum of the components that are
//! actually measured on this machine:
//! * CPU package: RAPL package zones; if RAPL is not readable, the SoC
//!   package power (PPT) of an AMD APU; else hwmon CPU power sensors.
//! * GPUs: NVIDIA (NVML) power and discrete AMD/Intel GPU power. An APU's
//!   PPT is never added here because it already covers the CPU package.
//! * Memory: RAPL DRAM zones (Intel platforms only).
//!
//! Values older than three sampling intervals are ignored, so a sleeping
//! dGPU (not polled) or a disabled collector simply drops out of the sum.

use crate::hub::Hub;
use crate::model::{DescCache, Health, Out, SeriesDesc};
use crate::settings::Settings;
use crate::util::time::now_ms;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// A component found in the catalog: (key, collector, label).
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub key: String,
    pub label: String,
    pub value: f64,
}

#[derive(Debug, Default, PartialEq)]
pub struct PowerSummary {
    pub cpu: Vec<Part>,
    pub gpu: Vec<Part>,
    pub dram: Vec<Part>,
}

impl PowerSummary {
    fn sum(v: &[Part]) -> Option<f64> {
        if v.is_empty() {
            None
        } else {
            Some(v.iter().map(|p| p.value).sum())
        }
    }

    pub fn total(&self) -> Option<f64> {
        let parts: Vec<f64> = [
            Self::sum(&self.cpu),
            Self::sum(&self.gpu),
            Self::sum(&self.dram),
        ]
        .into_iter()
        .flatten()
        .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.iter().sum())
        }
    }

    pub fn describe(&self) -> String {
        self.cpu
            .iter()
            .chain(&self.gpu)
            .chain(&self.dram)
            .map(|p| p.label.clone())
            .collect::<Vec<_>>()
            .join(" + ")
    }
}

/// Pick the measured components. `fresh(key, collector)` returns the latest
/// value when it is recent enough.
pub fn summarize(
    catalog: &[Arc<SeriesDesc>],
    fresh: &dyn Fn(&SeriesDesc) -> Option<f64>,
) -> PowerSummary {
    let mut s = PowerSummary::default();
    let part = |d: &SeriesDesc, label: String| {
        fresh(d).map(|v| Part {
            key: d.key.clone(),
            label,
            value: v,
        })
    };

    // CPU package from RAPL (top-level package zones only).
    for d in catalog.iter().filter(|d| d.collector == "power") {
        if let Some(zone) = d.key.strip_prefix("power.rapl.") {
            if zone.starts_with("package") && !zone.contains('.') {
                s.cpu.extend(part(d, format!("{} (RAPL)", d.label)));
            } else if zone.ends_with(".dram") || zone == "dram" {
                s.dram.extend(part(d, format!("{} (RAPL)", d.label)));
            }
        }
    }
    if s.cpu.is_empty()
        && let Some(p) = catalog
            .iter()
            .filter(|d| d.collector == "gpu_drm" && d.metric == "ppt")
            .find_map(|d| part(d, "SoC package (PPT)".into()))
    {
        s.cpu.push(p);
    }
    if s.cpu.is_empty() {
        for d in catalog
            .iter()
            .filter(|d| d.collector == "power" && d.key.starts_with("power.hwmon."))
        {
            s.cpu.extend(part(d, d.label.clone()));
        }
    }
    for d in catalog.iter().filter(|d| d.metric == "power") {
        if d.collector == "gpu_nvidia" || d.collector == "gpu_drm" {
            s.gpu.extend(part(d, d.group_label.clone()));
        }
    }
    s
}

pub async fn run(
    hub: Arc<Hub>,
    settings: watch::Receiver<Arc<Settings>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut descs = DescCache::default();
    loop {
        let iv = settings.borrow().collector("power").interval_ms.max(1000);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(iv)) => {}
            _ = shutdown.changed() => break,
        }
        let s = settings.borrow().clone();
        let now = now_ms();
        let (_, catalog) = hub.catalog();
        let fresh = |d: &SeriesDesc| {
            let (ts, v) = hub.latest(&d.key)?;
            let max_age = (s.collector(d.collector).interval_ms as i64 * 3).max(6000);
            (now - ts <= max_age).then_some(v)
        };
        let sum = summarize(&catalog, &fresh);
        let Some(total) = sum.total() else {
            continue;
        };
        let mut out = Out::new();
        let mut push = |key: &str, metric: &str, label: &str, v: Option<f64>| {
            if let Some(v) = v {
                let d = descs.get(key, || {
                    SeriesDesc::new("power_summary", key, metric, label, "W")
                });
                out.push(&d, v);
            }
        };
        push("power.total", "total", "Measured total", Some(total));
        push(
            "power.cpu_total",
            "cpu",
            "CPU package",
            PowerSummary::sum(&sum.cpu),
        );
        push(
            "power.gpu_total",
            "gpu",
            "GPUs",
            PowerSummary::sum(&sum.gpu),
        );
        push(
            "power.dram_total",
            "dram",
            "Memory",
            PowerSummary::sum(&sum.dram),
        );
        out.info("parts", sum.describe());
        out.health = Health::ok("derived");
        hub.publish("power_summary", now, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(collector: &'static str, key: &str, metric: &str, label: &str) -> Arc<SeriesDesc> {
        Arc::new(SeriesDesc::new(collector, key, metric, label, "W").group("g", "Group"))
    }

    #[test]
    fn rapl_wins_over_ppt_and_gpus_add_up() {
        let cat = vec![
            d("power", "power.rapl.package-0", "power", "CPU package 0"),
            d("power", "power.rapl.package-0.core", "power", "CPU cores"),
            d(
                "gpu_drm",
                "gpu.0000-07-00.0.ppt",
                "ppt",
                "SoC package power (PPT)",
            ),
            d("gpu_nvidia", "gpu.0000-01-00.0.power", "power", "Power"),
        ];
        let vals = |k: &str| match k {
            "power.rapl.package-0" => Some(40.0),
            "power.rapl.package-0.core" => Some(30.0),
            "gpu.0000-07-00.0.ppt" => Some(42.0),
            "gpu.0000-01-00.0.power" => Some(20.0),
            _ => None,
        };
        let s = summarize(&cat, &|x: &SeriesDesc| vals(&x.key));
        assert_eq!(s.cpu.len(), 1);
        assert_eq!(s.total(), Some(60.0));
    }

    #[test]
    fn ppt_fallback_and_stale_gpu() {
        let cat = vec![
            d("power", "power.rapl.package-0", "power", "CPU package 0"),
            d(
                "gpu_drm",
                "gpu.0000-07-00.0.ppt",
                "ppt",
                "SoC package power (PPT)",
            ),
            d("gpu_nvidia", "gpu.0000-01-00.0.power", "power", "Power"),
        ];
        // RAPL unreadable (no value), dGPU asleep (stale -> None)
        let vals = |k: &str| match k {
            "gpu.0000-07-00.0.ppt" => Some(12.5),
            _ => None,
        };
        let s = summarize(&cat, &|x: &SeriesDesc| vals(&x.key));
        assert_eq!(s.cpu[0].label, "SoC package (PPT)");
        assert!(s.gpu.is_empty());
        assert_eq!(s.total(), Some(12.5));
        assert_eq!(summarize(&[], &|_| None).total(), None);
    }
}
