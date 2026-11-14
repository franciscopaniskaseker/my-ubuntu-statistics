//! Generic hwmon sensors (/sys/class/hwmon): temperatures, fans, voltages,
//! currents, power and frequencies of every chip the kernel exposes.
//!
//! Series keys combine the chip name, a stable device id (PCI address, i2c
//! address or platform device name taken from the resolved `device` link) and
//! the sensor channel, because `hwmonN` numbers change between boots.

use super::{Collector, Ctx};
use crate::model::{DescCache, Health, Out, SeriesDesc};
use crate::util::parse::{dev_slug, slug};
use crate::util::sysfs::{Roots, list_dir, read_opt, read_opt_f64, read_str, resolve};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Sensor {
    pub kind: &'static str,
    pub index: u32,
    pub input: PathBuf,
    pub label: String,
    pub max: Option<f64>,
    pub crit: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct Chip {
    pub name: String,
    pub dev_id: String,
    pub dev_path: Option<PathBuf>,
    pub group_label: String,
    pub sensors: Vec<Sensor>,
}

impl Chip {
    pub fn group_id(&self) -> String {
        format!("{}.{}", slug(&self.name), dev_slug(&self.dev_id))
    }

    pub fn is_power_supply(&self) -> bool {
        self.dev_path
            .as_ref()
            .is_some_and(|p| p.to_string_lossy().contains("/power_supply/"))
    }

    pub fn is_gpu(&self) -> bool {
        self.dev_path
            .as_ref()
            .is_some_and(|p| p.join("drm").is_dir())
    }
}

const KINDS: &[&str] = &["temp", "fan", "in", "curr", "power", "freq", "humidity"];

pub fn unit_of(kind: &str) -> &'static str {
    match kind {
        "temp" => "C",
        "fan" => "rpm",
        "in" => "V",
        "curr" => "A",
        "power" => "W",
        "freq" => "MHz",
        "humidity" => "%",
        _ => "count",
    }
}

/// Convert a raw sysfs value to the display unit; `None` when implausible.
pub fn scale(kind: &str, raw: f64) -> Option<f64> {
    let v = match kind {
        "temp" => raw / 1000.0,
        "fan" => raw,
        "in" | "curr" | "humidity" => raw / 1000.0,
        "power" => raw / 1_000_000.0,
        "freq" => raw / 1_000_000.0,
        _ => raw,
    };
    let ok = match kind {
        "temp" => (-50.0..=200.0).contains(&v),
        "fan" => (0.0..50_000.0).contains(&v),
        "in" => (-100.0..=1000.0).contains(&v),
        "curr" => (-1000.0..=1000.0).contains(&v),
        "power" => (0.0..=10_000.0).contains(&v),
        "freq" => (0.0..=100_000.0).contains(&v),
        "humidity" => (0.0..=100.0).contains(&v),
        _ => true,
    };
    if ok && v.is_finite() { Some(v) } else { None }
}

/// Stable id for a device path: the i2c address, the last PCI address in the
/// path, or the last path component.
pub fn device_id(canon: &Path) -> String {
    let comps: Vec<String> = canon
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(|s| s.to_string()))
        .collect();
    let last = comps.last().cloned().unwrap_or_default();
    if is_i2c(&last) {
        return last;
    }
    if let Some(pci) = comps.iter().rev().find(|c| is_pci(c)) {
        return pci.clone();
    }
    last
}

fn is_pci(s: &str) -> bool {
    // dddd:bb:dd.f
    let b = s.as_bytes();
    b.len() == 12
        && b[4] == b':'
        && b[7] == b':'
        && b[10] == b'.'
        && s.chars()
            .enumerate()
            .all(|(i, c)| [4, 7, 10].contains(&i) || c.is_ascii_hexdigit())
}

fn is_i2c(s: &str) -> bool {
    match s.split_once('-') {
        Some((bus, addr)) => {
            !bus.is_empty()
                && bus.chars().all(|c| c.is_ascii_digit())
                && addr.len() == 4
                && addr.chars().all(|c| c.is_ascii_hexdigit())
        }
        None => false,
    }
}

fn friendly(name: &str, dev_id: &str, dev_path: Option<&Path>) -> String {
    let model = dev_path.and_then(|p| read_opt(&p.join("model")));
    match name {
        "k10temp" | "coretemp" | "zenpower" => format!("CPU ({name})"),
        "nvme" => match model {
            Some(m) => format!("NVMe {m}"),
            None => format!("NVMe {dev_id}"),
        },
        "drivetemp" => match model {
            Some(m) => format!("Disk {m}"),
            None => format!("Disk {dev_id}"),
        },
        "spd5118" | "jc42" | "ee1004" => format!("Memory DIMM {dev_id}"),
        "acpitz" => "ACPI thermal zone".to_string(),
        "acpi_fan" => "ACPI fan".to_string(),
        "hp" => "HP embedded controller".to_string(),
        "thinkpad" => "ThinkPad embedded controller".to_string(),
        "dell_smm" => "Dell embedded controller".to_string(),
        n if n.starts_with("nct") || n.starts_with("it87") || n.starts_with("it86") => {
            format!("Motherboard ({n})")
        }
        n if n.starts_with("iwlwifi")
            || n.starts_with("mt7")
            || n.starts_with("ath")
            || n.starts_with("rtw") =>
        {
            format!("Wi-Fi ({n})")
        }
        _ => format!("{name} ({dev_id})"),
    }
}

/// Parse a channel file name like `temp3_input` into ("temp", 3).
fn channel(fname: &str, suffix: &str) -> Option<(&'static str, u32)> {
    let base = fname.strip_suffix(suffix)?;
    for k in KINDS {
        if let Some(n) = base.strip_prefix(k)
            && let Ok(i) = n.parse::<u32>()
        {
            return Some((k, i));
        }
    }
    None
}

/// Enumerate every hwmon chip and its readable channels.
pub fn scan(roots: &Roots) -> Vec<Chip> {
    let mut chips = Vec::new();
    let mut seen: HashMap<(String, String), u32> = HashMap::new();
    for (_, dir) in list_dir(&roots.sys("class/hwmon")) {
        let Some(name) = read_opt(&dir.join("name")) else {
            continue;
        };
        let dev_path = resolve(&dir.join("device"));
        let mut dev_id = match &dev_path {
            Some(p) => device_id(p),
            None => "virtual".to_string(),
        };
        let n = seen.entry((name.clone(), dev_id.clone())).or_insert(0);
        *n += 1;
        if *n > 1 {
            dev_id = format!("{dev_id}-{n}");
        }
        // Some drivers keep attributes in the device dir instead of hwmonN.
        let attr_dir = match &dev_path {
            Some(p) if !has_inputs(&dir) && has_inputs(p) => p.clone(),
            _ => dir.clone(),
        };
        let mut sensors = Vec::new();
        let files = list_dir(&attr_dir);
        for (fname, path) in &files {
            let (kind, idx, is_avg) = match channel(fname, "_input") {
                Some((k, i)) => (k, i, false),
                None => match channel(fname, "_average") {
                    Some(("power", i)) => ("power", i, true),
                    _ => continue,
                },
            };
            if is_avg && attr_dir.join(format!("power{idx}_input")).exists() {
                continue;
            }
            let label = read_opt(&attr_dir.join(format!("{kind}{idx}_label")))
                .unwrap_or_else(|| format!("{kind}{idx}"));
            let lim = |s: &str| {
                read_opt_f64(&attr_dir.join(format!("{kind}{idx}_{s}")))
                    .and_then(|v| scale(kind, v))
                    .filter(|v| kind != "temp" || (20.0..=200.0).contains(v))
            };
            sensors.push(Sensor {
                kind,
                index: idx,
                input: path.clone(),
                label,
                max: lim("max"),
                crit: lim("crit"),
            });
        }
        let group_label = friendly(&name, &dev_id, dev_path.as_deref());
        chips.push(Chip {
            name,
            dev_id,
            dev_path,
            group_label,
            sensors,
        });
    }
    chips
}

fn has_inputs(dir: &Path) -> bool {
    list_dir(dir)
        .iter()
        .any(|(f, _)| f.ends_with("_input") || f.ends_with("_average"))
}

/// Read and scale one sensor. `Err` = read failed, `Ok(None)` = implausible.
pub fn read_sensor(s: &Sensor) -> std::io::Result<Option<f64>> {
    let raw = read_str(&s.input)?;
    let v = raw
        .parse::<f64>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(scale(s.kind, v))
}

/// Reads slower than this mark a sensor as slow (e.g. firmware/WMI fans).
const SLOW_READ: Duration = Duration::from_millis(50);
/// Slow sensors are read at most this often.
pub const SLOW_PERIOD: Duration = Duration::from_secs(10);

pub struct Hwmon {
    roots: Roots,
    descs: DescCache,
    chips: Vec<Chip>,
    last_scan: Option<Instant>,
    /// Slow sensors (by input path) and when they were last read.
    slow: HashMap<PathBuf, Option<Instant>>,
}

impl Hwmon {
    pub fn new(roots: Roots) -> Self {
        Self {
            roots,
            descs: DescCache::default(),
            chips: Vec::new(),
            last_scan: None,
            slow: HashMap::new(),
        }
    }
}

impl Collector for Hwmon {
    fn collect(&mut self, ctx: &Ctx, out: &mut Out) -> anyhow::Result<()> {
        if self
            .last_scan
            .is_none_or(|t| ctx.now.duration_since(t) > Duration::from_secs(30))
        {
            self.chips = scan(&self.roots)
                .into_iter()
                .filter(|c| !c.is_power_supply() && !c.is_gpu() && !c.sensors.is_empty())
                .collect();
            self.last_scan = Some(ctx.now);
        }
        let mut ok = 0usize;
        let mut failed = 0usize;
        let mut slow_count = 0usize;
        let period_ms = (SLOW_PERIOD.as_millis() as u64).max(ctx.interval.as_millis() as u64);
        for chip in &self.chips {
            let gid = chip.group_id();
            for s in &chip.sensors {
                let is_slow = self.slow.contains_key(&s.input);
                if is_slow {
                    slow_count += 1;
                    if let Some(Some(t)) = self.slow.get(&s.input)
                        && ctx.now.duration_since(*t) < SLOW_PERIOD
                    {
                        ok += 1;
                        continue;
                    }
                }
                let t0 = Instant::now();
                let res = read_sensor(s);
                let took = t0.elapsed();
                if is_slow {
                    self.slow.insert(s.input.clone(), Some(ctx.now));
                } else if took > SLOW_READ {
                    tracing::info!(
                        "sensor {} {} takes {} ms to read; reading it every {} s",
                        chip.name,
                        s.label,
                        took.as_millis(),
                        SLOW_PERIOD.as_secs()
                    );
                    self.slow.insert(s.input.clone(), Some(ctx.now));
                    slow_count += 1;
                }
                let v = match res {
                    Ok(Some(v)) => v,
                    Ok(None) => continue,
                    Err(_) => {
                        failed += 1;
                        continue;
                    }
                };
                ok += 1;
                let key = format!("hwmon.{gid}.{}{}", s.kind, s.index);
                let slow = self.slow.contains_key(&s.input);
                let make = || {
                    let mut d = SeriesDesc::new(
                        "sensors",
                        key.clone(),
                        s.kind,
                        s.label.clone(),
                        unit_of(s.kind),
                    )
                    .group(gid.clone(), chip.group_label.clone())
                    .limits(s.max, s.crit);
                    if slow {
                        d.period_ms = Some(period_ms);
                    }
                    d
                };
                let mut d = self.descs.get(&key, make);
                if d.period_ms.is_some() != slow {
                    d = self.descs.put(make());
                }
                out.push(&d, v);
            }
        }
        out.health = if ok > 0 && failed == 0 && slow_count > 0 {
            let mut h = Health::ok("/sys/class/hwmon");
            h.reason = Some(format!(
                "{slow_count} slow sensor(s) read every {} s",
                SLOW_PERIOD.as_secs()
            ));
            h
        } else if ok > 0 && failed == 0 {
            Health::ok("/sys/class/hwmon")
        } else if ok > 0 {
            Health::degraded(
                "/sys/class/hwmon",
                format!("{failed} sensor reads failed this tick"),
            )
        } else {
            Health::unavailable("no readable hwmon sensors").hint(
                "load the sensor driver for your hardware (e.g. coretemp/k10temp, drivetemp for SATA disks, or run sensors-detect from lm-sensors)",
            )
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids() {
        assert_eq!(
            device_id(Path::new(
                "/sys/devices/pci0000:00/0000:00:01.2/0000:02:00.0/nvme/nvme1"
            )),
            "0000:02:00.0"
        );
        assert_eq!(
            device_id(Path::new(
                "/sys/devices/pci0000:00/0000:00:14.0/i2c-1/1-0050"
            )),
            "1-0050"
        );
        assert_eq!(
            device_id(Path::new("/sys/devices/platform/hp-wmi")),
            "hp-wmi"
        );
        assert_eq!(
            device_id(Path::new("/sys/devices/pci0000:00/0000:00:18.3")),
            "0000:00:18.3"
        );
    }

    #[test]
    fn scaling_and_sanity() {
        assert_eq!(scale("temp", 45000.0), Some(45.0));
        assert_eq!(scale("temp", -273150.0), None);
        assert_eq!(scale("temp", 255500.0), None);
        assert_eq!(scale("fan", 65535.0), None);
        assert_eq!(scale("fan", 2800.0), Some(2800.0));
        assert_eq!(scale("power", 15_000_000.0), Some(15.0));
        assert_eq!(scale("in", 1200.0), Some(1.2));
    }

    #[test]
    fn fixture_scan_keys_are_unique_and_skip_eio() {
        let roots = Roots::fixture("laptop");
        let mut h = Hwmon::new(roots);
        let settings = crate::settings::Settings::default();
        let ctx = Ctx {
            now: Instant::now(),
            wall_ms: 0,
            interval: Duration::from_secs(1),
            settings: &settings,
        };
        let mut out = Out::new();
        h.collect(&ctx, &mut out).unwrap();
        let keys: Vec<&str> = out.points.iter().map(|(d, _)| d.key.as_str()).collect();
        assert!(keys.contains(&"hwmon.nvme.0000-02-00.0.temp1"), "{keys:?}");
        assert!(keys.contains(&"hwmon.nvme.0000-06-00.0.temp1"), "{keys:?}");
        assert!(keys.contains(&"hwmon.spd5118.1-0050.temp1"), "{keys:?}");
        assert!(keys.contains(&"hwmon.spd5118.1-0051.temp1"), "{keys:?}");
        assert!(keys.contains(&"hwmon.hp.hp-wmi.fan1"), "{keys:?}");
        // bogus -273 C sensor is dropped, battery chip skipped, GPU chip skipped
        assert!(!keys.iter().any(|k| k.contains("bat0")));
        assert!(!keys.iter().any(|k| k.contains("amdgpu")));
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), keys.len());
        let nvme = out
            .points
            .iter()
            .find(|(d, _)| d.key == "hwmon.nvme.0000-02-00.0.temp1")
            .unwrap();
        assert_eq!(nvme.0.group_label, "NVMe Example SSD 2TB");
        assert_eq!(nvme.0.crit, Some(84.85));
    }
}
