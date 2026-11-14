//! Non-NVIDIA-proprietary GPUs through DRM sysfs: amdgpu (load, video
//! engine, VRAM, shared memory, clocks), Intel i915/xe (frequency) and the
//! card's hwmon sensors (temperature, power, clock, voltages).
//!
//! On AMD APUs (integrated Radeon) the hwmon `PPT` power sensor reports the
//! whole SoC package (CPU + iGPU); it is exported as metric `ppt`, never as
//! GPU-only power. Runtime-suspended GPUs are not touched.

use super::hwmon::{self, Sensor};
use super::{Collector, Ctx};
use crate::model::{Desc, DescCache, Health, Out, SeriesDesc};
use crate::util::parse::dev_slug;
use crate::util::pciids;
use crate::util::sysfs::{
    Roots, link_name, list_dir, read_opt, read_opt_f64, read_opt_u64, resolve, runtime_suspended,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// AMD GPUs with at most this much dedicated VRAM are treated as integrated.
const IGPU_MAX_VRAM: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone)]
struct Card {
    card_dir: PathBuf,
    dev_dir: PathBuf,
    addr: String,
    vendor: String,
    driver: String,
    name: String,
    integrated: bool,
    sensors: Vec<Sensor>,
}

pub struct GpuDrm {
    roots: Roots,
    descs: DescCache,
    cards: Vec<Card>,
    last_scan: Option<Instant>,
}

/// Current level of an amdgpu `pp_dpm_*` table ("2: 2600Mhz *") in MHz.
pub fn current_dpm_mhz(s: &str) -> Option<f64> {
    s.lines()
        .find(|l| l.trim_end().ends_with('*'))
        .and_then(|l| {
            let v = l.split_once(':')?.1.trim().trim_end_matches('*').trim();
            let num: String = v
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            let n = num.parse::<f64>().ok()?;
            Some(if v.to_ascii_lowercase().contains("ghz") {
                n * 1000.0
            } else {
                n
            })
        })
}

fn vendor_name(v: &str) -> &'static str {
    match v {
        "0x1002" => "AMD",
        "0x8086" => "Intel",
        "0x10de" => "NVIDIA",
        _ => "Unknown",
    }
}

fn friendly_name(dev_dir: &Path, vendor: &str, integrated: bool) -> String {
    let hex = |s: Option<String>| {
        s.and_then(|v| u16::from_str_radix(v.trim_start_matches("0x"), 16).ok())
    };
    let vid = hex(Some(vendor.to_string()));
    let did = hex(read_opt(&dev_dir.join("device")));
    let mut name = read_opt(&dev_dir.join("product_name"))
        .or_else(|| match (vid, did) {
            (Some(v), Some(d)) => pciids::lookup(v, d),
            _ => None,
        })
        .unwrap_or_else(|| format!("{} GPU", vendor_name(vendor)));
    if vendor == "0x1002" && !name.contains("Radeon") {
        name = if name.starts_with("AMD ") {
            name.replacen("AMD ", "AMD Radeon ", 1)
        } else {
            format!("AMD Radeon {name}")
        };
    }
    if integrated {
        format!("{name} (integrated)")
    } else {
        name
    }
}

impl GpuDrm {
    pub fn new(roots: Roots) -> Self {
        Self {
            roots,
            descs: DescCache::default(),
            cards: Vec::new(),
            last_scan: None,
        }
    }

    fn scan(&mut self) {
        let chips = hwmon::scan(&self.roots);
        self.cards.clear();
        for (name, card_dir) in list_dir(&self.roots.sys("class/drm")) {
            let Some(rest) = name.strip_prefix("card") else {
                continue;
            };
            if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let Some(dev_dir) = resolve(&card_dir.join("device")) else {
                continue;
            };
            let driver = link_name(&dev_dir.join("driver")).unwrap_or_default();
            if driver == "nvidia" || driver.is_empty() {
                continue;
            }
            let addr = dev_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&name)
                .to_string();
            if self.cards.iter().any(|c| c.addr == addr) {
                continue;
            }
            let vendor = read_opt(&dev_dir.join("vendor")).unwrap_or_default();
            let vram = read_opt_u64(&dev_dir.join("mem_info_vram_total"));
            let integrated = match vendor.as_str() {
                "0x1002" => vram.is_none_or(|v| v <= IGPU_MAX_VRAM),
                "0x8086" => vram.is_none(),
                _ => false,
            };
            let sensors = chips
                .iter()
                .filter(|c| c.dev_path.as_deref() == Some(dev_dir.as_path()))
                .flat_map(|c| c.sensors.clone())
                .collect();
            self.cards.push(Card {
                name: friendly_name(&dev_dir, &vendor, integrated),
                card_dir,
                dev_dir,
                addr,
                vendor,
                driver,
                integrated,
                sensors,
            });
        }
    }

    fn desc(&mut self, card: &Card, metric: &str, label: &str, unit: &'static str) -> Desc {
        let id = dev_slug(&card.addr);
        let key = format!("gpu.{id}.{metric}");
        let make = || {
            SeriesDesc::new("gpu_drm", key.clone(), metric, label, unit)
                .group(id.clone(), card.name.clone())
        };
        let d = self.descs.get(&key, make);
        if d.group_label != card.name || d.label != label {
            return self.descs.put(make());
        }
        d
    }
}

impl Collector for GpuDrm {
    fn collect(&mut self, ctx: &Ctx, out: &mut Out) -> anyhow::Result<()> {
        if self
            .last_scan
            .is_none_or(|t| ctx.now.duration_since(t) > Duration::from_secs(60))
        {
            self.scan();
            self.last_scan = Some(ctx.now);
        }
        if self.cards.is_empty() {
            out.health = Health::unavailable("no AMD/Intel/nouveau GPU found");
            return Ok(());
        }
        for card in self.cards.clone() {
            let slug = dev_slug(&card.addr);
            let sleeping = runtime_suspended(&card.dev_dir);
            let d = self.desc(&card, "active", "Powered on", "bool");
            out.push(&d, if sleeping { 0.0 } else { 1.0 });
            out.info(format!("{slug}.driver"), card.driver.clone());
            out.info(
                format!("{slug}.state"),
                if sleeping { "sleeping" } else { "active" },
            );
            out.info(
                format!("{slug}.kind"),
                if card.integrated {
                    "integrated"
                } else {
                    "discrete"
                },
            );
            if sleeping {
                continue;
            }
            let f = |n: &str| read_opt_f64(&card.dev_dir.join(n));
            let mut vals: Vec<(&str, &str, &'static str, f64)> = Vec::new();
            if let Some(v) = f("gpu_busy_percent") {
                vals.push(("util", "GPU load", "%", v));
            }
            if let Some(v) = f("mem_busy_percent") {
                vals.push(("mem_util", "Memory controller", "%", v));
            }
            if let Some(v) = f("vcn_busy_percent") {
                vals.push(("video", "Video engine", "%", v));
            }
            if let Some(v) = f("mem_info_vram_used") {
                vals.push(("vram_used", "VRAM used", "B", v));
            }
            if let Some(v) = f("mem_info_vram_total") {
                vals.push(("vram_total", "VRAM total", "B", v));
            }
            if let Some(v) = f("mem_info_gtt_used") {
                vals.push(("gtt_used", "Shared memory used", "B", v));
            }
            if let Some(v) = f("mem_info_gtt_total") {
                vals.push(("gtt_total", "Shared memory total", "B", v));
            }
            let dpm = |n: &str| {
                std::fs::read_to_string(card.dev_dir.join(n))
                    .ok()
                    .and_then(|s| current_dpm_mhz(&s))
            };
            if let Some(v) = dpm("pp_dpm_mclk") {
                vals.push(("clock_mem", "Memory clock", "MHz", v));
            }
            if let Some(v) = dpm("pp_dpm_fclk") {
                vals.push(("clock_fabric", "Fabric clock", "MHz", v));
            }
            if card.vendor == "0x8086" {
                let freq = read_opt_f64(&card.card_dir.join("gt_act_freq_mhz"))
                    .or_else(|| read_opt_f64(&card.card_dir.join("gt/gt0/rps_act_freq_mhz")))
                    .or_else(|| read_opt_f64(&card.dev_dir.join("tile0/gt0/freq0/act_freq")));
                if let Some(v) = freq {
                    vals.push(("clock_gr", "Core clock", "MHz", v));
                }
            }
            let mut have_clock = vals.iter().any(|v| v.0 == "clock_gr");
            let (mut have_temp, mut have_power) = (false, false);
            for s in &card.sensors {
                let Ok(Some(v)) = hwmon::read_sensor(s) else {
                    continue;
                };
                match s.kind {
                    "temp" if !have_temp => {
                        have_temp = true;
                        vals.push(("temp", "Temperature", "C", v));
                    }
                    "power" if !have_power => {
                        have_power = true;
                        if card.integrated && s.label.eq_ignore_ascii_case("ppt") {
                            vals.push(("ppt", "SoC package power (PPT)", "W", v));
                        } else {
                            vals.push(("power", "Power", "W", v));
                        }
                    }
                    "freq" if !have_clock => {
                        have_clock = true;
                        vals.push(("clock_gr", "Core clock", "MHz", v));
                    }
                    "in" if s.label.eq_ignore_ascii_case("vddgfx") => {
                        vals.push(("voltage", "Core voltage", "V", v));
                    }
                    "in" if s.label.eq_ignore_ascii_case("vddnb") => {
                        vals.push(("voltage_soc", "SoC voltage", "V", v));
                    }
                    _ => {}
                }
            }
            for (metric, label, unit, v) in vals {
                let d = self.desc(&card, metric, label, unit);
                out.push(&d, v);
            }
        }
        out.health = Health::ok("DRM sysfs + hwmon");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpm_parse() {
        assert_eq!(
            current_dpm_mhz("0: 1000Mhz \n1: 1600Mhz \n2: 2600Mhz *\n"),
            Some(2600.0)
        );
        assert_eq!(current_dpm_mhz("0: 400Mhz\n1: 1.2Ghz *\n"), Some(1200.0));
        assert_eq!(current_dpm_mhz(""), None);
        assert_eq!(current_dpm_mhz("0: 400Mhz\n"), None);
    }
}
