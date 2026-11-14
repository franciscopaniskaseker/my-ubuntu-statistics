//! Batteries, AC adapters and USB-C power supplies (/sys/class/power_supply).
//!
//! Handles both the energy (µWh/µW) and the charge (µAh/µA) flavours of the
//! battery interface and firmware that reports negative power.

use super::{Collector, Ctx};
use crate::model::{Desc, DescCache, Health, Out, SeriesDesc};
use crate::util::parse::slug;
use crate::util::sysfs::{Roots, list_dir, read_opt, read_opt_f64};
use std::path::Path;

pub struct Battery {
    roots: Roots,
    descs: DescCache,
}

#[derive(Debug, Default, PartialEq)]
pub struct BatteryReading {
    pub capacity: Option<f64>,
    pub power_w: Option<f64>,
    pub voltage_v: Option<f64>,
    pub energy_wh: Option<f64>,
    pub energy_full_wh: Option<f64>,
    pub health_pct: Option<f64>,
    pub cycles: Option<f64>,
    pub status: Option<String>,
    pub used_charge_fallback: bool,
}

/// Read one battery directory.
pub fn read_battery(dir: &Path) -> BatteryReading {
    let f = |n: &str| read_opt_f64(&dir.join(n));
    let mut r = BatteryReading {
        status: read_opt(&dir.join("status")),
        voltage_v: f("voltage_now").map(|v| v / 1e6).filter(|v| *v > 0.0),
        cycles: f("cycle_count").filter(|v| *v > 0.0),
        ..Default::default()
    };
    let v_design = f("voltage_min_design").map(|v| v / 1e6).or(r.voltage_v);

    r.power_w = match f("power_now") {
        Some(p) => Some(p.abs() / 1e6),
        None => match (f("current_now"), r.voltage_v) {
            (Some(i), Some(v)) => {
                r.used_charge_fallback = true;
                Some(i.abs() / 1e6 * v)
            }
            _ => None,
        },
    };

    let (now, full, design) = match (f("energy_now"), f("energy_full")) {
        (Some(n), full) => (
            Some(n / 1e6),
            full.map(|x| x / 1e6),
            f("energy_full_design").map(|x| x / 1e6),
        ),
        (None, _) => {
            let conv = |x: Option<f64>| match (x, v_design) {
                (Some(c), Some(v)) => Some(c / 1e6 * v),
                _ => None,
            };
            let n = conv(f("charge_now"));
            if n.is_some() {
                r.used_charge_fallback = true;
            }
            (n, conv(f("charge_full")), conv(f("charge_full_design")))
        }
    };
    r.energy_wh = now;
    r.energy_full_wh = full;
    if let (Some(fu), Some(d)) = (full, design)
        && d > 0.0
    {
        r.health_pct = Some((fu / d * 100.0).clamp(0.0, 150.0));
    }
    r.capacity = f("capacity")
        .or_else(|| match (now, full) {
            (Some(n), Some(fu)) if fu > 0.0 => Some(n / fu * 100.0),
            _ => None,
        })
        .map(|c| c.clamp(0.0, 100.0));
    r
}

impl Battery {
    pub fn new(roots: Roots) -> Self {
        Self {
            roots,
            descs: DescCache::default(),
        }
    }

    fn desc(
        &mut self,
        id: &str,
        glabel: &str,
        metric: &str,
        label: &str,
        unit: &'static str,
    ) -> Desc {
        let key = format!("{id}.{metric}");
        self.descs.get(&key, || {
            SeriesDesc::new("battery", key.clone(), metric, label, unit).group(id, glabel)
        })
    }
}

impl Collector for Battery {
    fn collect(&mut self, _ctx: &Ctx, out: &mut Out) -> anyhow::Result<()> {
        let mut found = 0;
        let mut fallback = false;
        for (name, dir) in list_dir(&self.roots.sys("class/power_supply")) {
            let ty = read_opt(&dir.join("type")).unwrap_or_default();
            match ty.as_str() {
                "Battery" => {
                    if read_opt(&dir.join("present")).as_deref() == Some("0") {
                        continue;
                    }
                    // Peripheral batteries (mice, headsets) report scope=Device.
                    if read_opt(&dir.join("scope")).as_deref() == Some("Device") {
                        continue;
                    }
                    found += 1;
                    let r = read_battery(&dir);
                    fallback |= r.used_charge_fallback;
                    let id = format!("bat.{}", slug(&name));
                    let model = read_opt(&dir.join("model_name"));
                    let glabel = match model {
                        Some(m) => format!("Battery {name} ({m})"),
                        None => format!("Battery {name}"),
                    };
                    let discharging = r.status.as_deref() == Some("Discharging");
                    let items = [
                        ("capacity", "Charge", "%", r.capacity),
                        ("power", "Power", "W", r.power_w),
                        ("voltage", "Voltage", "V", r.voltage_v),
                        ("energy", "Energy", "Wh", r.energy_wh),
                        ("energy_full", "Full capacity", "Wh", r.energy_full_wh),
                        ("health", "Health", "%", r.health_pct),
                        ("cycles", "Cycles", "count", r.cycles),
                    ];
                    for (metric, label, unit, v) in items {
                        if let Some(v) = v {
                            let d = self.desc(&id, &glabel, metric, label, unit);
                            out.push(&d, v);
                        }
                    }
                    // Signed power: negative while discharging.
                    if let Some(p) = r.power_w {
                        let d = self.desc(&id, &glabel, "power_signed", "Charge rate", "W");
                        out.push(&d, if discharging { -p } else { p });
                    }
                    if let (Some(e), Some(p)) = (r.energy_wh, r.power_w)
                        && discharging
                        && p > 0.5
                    {
                        let d = self.desc(&id, &glabel, "time_left", "Time remaining", "s");
                        out.push(&d, e / p * 3600.0);
                    }
                    if let Some(s) = r.status {
                        out.info(format!("{id}.status"), s);
                    }
                }
                "Mains" | "USB" | "USB_C" | "USB_PD" => {
                    let id = format!("psu.{}", slug(&name));
                    let glabel = if ty == "Mains" {
                        format!("AC adapter {name}")
                    } else {
                        format!("USB power {name}")
                    };
                    if let Some(on) = read_opt_f64(&dir.join("online")) {
                        found += 1;
                        let d = self.desc(&id, &glabel, "online", "Connected", "bool");
                        out.push(&d, if on > 0.0 { 1.0 } else { 0.0 });
                        if ty != "Mains" && on > 0.0 {
                            let v = read_opt_f64(&dir.join("voltage_now")).map(|v| v / 1e6);
                            let i = read_opt_f64(&dir.join("current_now")).map(|v| v / 1e6);
                            if let (Some(v), Some(i)) = (v, i) {
                                let d =
                                    self.desc(&id, &glabel, "contract_w", "Negotiated power", "W");
                                out.push(&d, (v * i).abs());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        out.health = if found == 0 {
            Health::unavailable("no battery or power supply found")
        } else if fallback {
            Health::degraded(
                "/sys/class/power_supply",
                "battery reports charge/current; power and energy derived from voltage",
            )
        } else {
            Health::ok("/sys/class/power_supply")
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn energy_flavour() {
        let roots = Roots::fixture("laptop");
        let r = read_battery(&roots.sys("class/power_supply/BAT0"));
        assert_eq!(r.capacity, Some(80.0));
        assert_eq!(r.power_w, Some(12.5));
        assert!((r.health_pct.unwrap() - 90.0).abs() < 1e-9);
        assert!(!r.used_charge_fallback);
    }

    #[test]
    fn charge_flavour_with_negative_current() {
        let roots = Roots::fixture("rhel8");
        let r = read_battery(&roots.sys("class/power_supply/BAT1"));
        assert!(r.used_charge_fallback);
        // 1.5 A * 12 V
        assert!((r.power_w.unwrap() - 18.0).abs() < 1e-9);
        // 3 Ah * 11.1 V
        assert!((r.energy_wh.unwrap() - 33.3).abs() < 1e-6);
        assert_eq!(r.capacity, Some(60.0));
    }
}
