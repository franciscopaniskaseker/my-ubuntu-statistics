//! NVIDIA GPUs via NVML (loaded at runtime), falling back to `nvidia-smi`.
//!
//! * A GPU whose PCI `power/runtime_status` is `suspended` is reported as
//!   sleeping and never queried, so the monitor does not wake a laptop dGPU.
//! * NVML initialisation is guarded by a marker file containing the boot id.
//!   If the process crashes inside NVML, the next start finds the marker and
//!   disables NVML for the rest of that boot (using `nvidia-smi` instead).
//! * Initialisation errors (driver not loaded, driver/library mismatch after
//!   an update) are retried every 5 minutes.

use super::{Collector, Ctx};
use crate::model::{Desc, DescCache, Health, Out, SeriesDesc};
use crate::util::parse::dev_slug;
use crate::util::sysfs::{Roots, link_name, list_dir, read_opt, runtime_suspended};
use nvml_wrapper::Nvml;
use nvml_wrapper::bitmasks::device::ThrottleReasons;
use nvml_wrapper::enum_wrappers::device::{Clock, PcieUtilCounter, TemperatureSensor};
use nvml_wrapper::enums::device::UsedGpuMemory;
use nvml_wrapper::error::NvmlError;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const RETRY: Duration = Duration::from_secs(300);
const SMI_TIMEOUT: Duration = Duration::from_secs(2);
const MARKER: &str = "nvml-init.marker";

#[derive(Debug, Clone)]
struct PciGpu {
    addr: String,
    dir: PathBuf,
    driver: Option<String>,
}

enum Backend {
    Uninit,
    Nvml(Box<Nvml>),
    Smi(PathBuf),
    None,
}

pub struct GpuNvidia {
    roots: Roots,
    descs: DescCache,
    gpus: Vec<PciGpu>,
    last_scan: Option<Instant>,
    backend: Backend,
    retry_at: Option<Instant>,
    note: Option<String>,
    hint: Option<String>,
    names: HashMap<String, String>,
    last_smi: Option<(Instant, Vec<SmiRow>)>,
    /// Static facts per GPU (CUDA cores, architecture, driver...).
    facts: HashMap<String, Vec<(String, String)>>,
    /// Last process-utilization sample timestamp per GPU (µs).
    proc_ts: HashMap<String, u64>,
}

/// NVIDIA display-class PCI devices from sysfs.
fn scan_pci(roots: &Roots) -> Vec<PciGpu> {
    list_dir(&roots.sys("bus/pci/devices"))
        .into_iter()
        .filter(|(_, d)| {
            read_opt(&d.join("vendor")).as_deref() == Some("0x10de")
                && read_opt(&d.join("class")).is_some_and(|c| c.starts_with("0x03"))
        })
        .map(|(addr, dir)| PciGpu {
            driver: link_name(&dir.join("driver")),
            addr,
            dir,
        })
        .collect()
}

fn boot_id(roots: &Roots) -> String {
    read_opt(&roots.proc("sys/kernel/random/boot_id")).unwrap_or_else(|| "unknown".into())
}

fn find_smi() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend(
        ["/usr/bin", "/usr/local/bin", "/bin", "/opt/nvidia/bin"]
            .iter()
            .map(PathBuf::from),
    );
    dirs.into_iter()
        .map(|d| d.join("nvidia-smi"))
        .find(|p| p.is_file())
}

/// Run a command with a hard timeout; the child is killed when it expires.
pub fn run_with_timeout(cmd: &Path, args: &[&str], timeout: Duration) -> std::io::Result<String> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let mut s = String::new();
            if let Some(mut o) = child.stdout.take() {
                o.read_to_string(&mut s)?;
            }
            if !status.success() {
                return Err(std::io::Error::other(format!("exit status {status}")));
            }
            return Ok(s);
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SmiRow {
    pub addr: String,
    pub name: String,
    pub vals: [Option<f64>; 9],
    pub pstate: Option<String>,
}

const SMI_FIELDS: &str = "pci.bus_id,name,utilization.gpu,utilization.memory,memory.used,memory.total,temperature.gpu,power.draw,clocks.gr,clocks.mem,fan.speed,pstate";

/// Normalize "00000000:01:00.0" to "0000:01:00.0".
fn norm_bus_id(s: &str) -> String {
    let s = s.trim().to_ascii_lowercase();
    match s.split_once(':') {
        Some((dom, rest)) if dom.len() > 4 => format!("{}:{rest}", &dom[dom.len() - 4..]),
        _ => s,
    }
}

pub fn parse_smi(s: &str) -> Vec<SmiRow> {
    s.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').map(|x| x.trim()).collect();
            if f.len() < 12 {
                return None;
            }
            let num = |x: &str| x.parse::<f64>().ok().filter(|v| v.is_finite());
            let mut vals = [None; 9];
            for (i, v) in vals.iter_mut().enumerate() {
                *v = num(f[2 + i]);
            }
            Some(SmiRow {
                addr: norm_bus_id(f[0]),
                name: f[1].to_string(),
                vals,
                pstate: Some(f[11].to_string()).filter(|p| p.starts_with('P')),
            })
        })
        .collect()
}

impl GpuNvidia {
    pub fn new(roots: Roots) -> Self {
        Self {
            roots,
            descs: DescCache::default(),
            gpus: Vec::new(),
            last_scan: None,
            backend: Backend::Uninit,
            retry_at: None,
            note: None,
            hint: None,
            names: HashMap::new(),
            last_smi: None,
            facts: HashMap::new(),
            proc_ts: HashMap::new(),
        }
    }

    fn desc(&mut self, gpu: &PciGpu, metric: &str, label: &str, unit: &'static str) -> Desc {
        let id = dev_slug(&gpu.addr);
        let key = format!("gpu.{id}.{metric}");
        let gl = self
            .names
            .get(&gpu.addr)
            .cloned()
            .unwrap_or_else(|| format!("NVIDIA GPU {}", gpu.addr));
        let make = || {
            SeriesDesc::new("gpu_nvidia", key.clone(), metric, label, unit)
                .group(id.clone(), gl.clone())
        };
        let d = self.descs.get(&key, make);
        if d.group_label != gl {
            return self.descs.put(make());
        }
        d
    }

    fn init_backend(&mut self) {
        self.note = None;
        self.hint = None;
        let marker = self.roots.state.join(MARKER);
        let boot = boot_id(&self.roots);
        if let Ok(prev) = std::fs::read_to_string(&marker) {
            if prev.trim() == boot {
                self.note = Some(
                    "NVML crashed during initialisation earlier in this boot; using nvidia-smi"
                        .into(),
                );
                self.backend = find_smi().map(Backend::Smi).unwrap_or(Backend::None);
                return;
            }
            let _ = std::fs::remove_file(&marker);
        }
        let _ = std::fs::write(&marker, &boot);
        let res = Nvml::builder()
            .lib_path(std::ffi::OsStr::new("libnvidia-ml.so.1"))
            .init()
            .or_else(|e| match e {
                NvmlError::LibloadingError(_) => Nvml::init(),
                e => Err(e),
            });
        let _ = std::fs::remove_file(&marker);
        match res {
            Ok(n) => {
                self.backend = Backend::Nvml(Box::new(n));
                self.retry_at = None;
            }
            Err(e) => {
                let (reason, hint) = match &e {
                    NvmlError::LibloadingError(_) | NvmlError::LibraryNotFound => (
                        "libnvidia-ml not found".to_string(),
                        Some("install the NVIDIA driver utilities (libnvidia-ml / nvidia-utils)"),
                    ),
                    NvmlError::DriverNotLoaded => (
                        "NVIDIA kernel driver not loaded".to_string(),
                        Some("load the nvidia kernel module or reboot after a driver update"),
                    ),
                    NvmlError::LibRmVersionMismatch => (
                        "NVIDIA driver/library version mismatch".to_string(),
                        Some("reboot to finish the NVIDIA driver update"),
                    ),
                    NvmlError::NoPermission => (
                        "no permission to access the NVIDIA driver".to_string(),
                        None,
                    ),
                    other => (format!("NVML init failed: {other}"), None),
                };
                self.hint = hint.map(String::from);
                self.retry_at = Some(Instant::now() + RETRY);
                match find_smi() {
                    Some(p) => {
                        self.note = Some(format!("{reason}; using nvidia-smi"));
                        self.backend = Backend::Smi(p);
                    }
                    None => {
                        self.note = Some(reason);
                        self.backend = Backend::None;
                    }
                }
            }
        }
    }

    fn query_nvml(&mut self, gpu: &PciGpu, out: &mut Out) -> Result<(), NvmlError> {
        let Backend::Nvml(nvml) = &self.backend else {
            return Ok(());
        };
        let dev = nvml
            .device_by_pci_bus_id(format!("0000{}", gpu.addr))
            .or_else(|_| nvml.device_by_pci_bus_id(gpu.addr.clone()))?;
        let fatal = |e: &NvmlError| {
            matches!(
                e,
                NvmlError::GpuLost
                    | NvmlError::Uninitialized
                    | NvmlError::DriverNotLoaded
                    | NvmlError::LibRmVersionMismatch
            )
        };
        let mut vals: Vec<(&str, &str, &'static str, f64)> = Vec::with_capacity(12);
        let name = dev.name().ok();
        match dev.utilization_rates() {
            Ok(u) => {
                vals.push(("util", "GPU load", "%", u.gpu as f64));
                vals.push(("mem_util", "Memory controller", "%", u.memory as f64));
            }
            Err(e) if fatal(&e) => return Err(e),
            Err(_) => {}
        }
        if let Ok(m) = dev.memory_info() {
            vals.push(("vram_used", "VRAM used", "B", m.used as f64));
            vals.push(("vram_total", "VRAM total", "B", m.total as f64));
        }
        if let Ok(t) = dev.temperature(TemperatureSensor::Gpu) {
            vals.push(("temp", "Temperature", "C", t as f64));
        }
        if let Ok(p) = dev.power_usage() {
            vals.push(("power", "Power", "W", p as f64 / 1000.0));
        }
        if let Ok(c) = dev.clock_info(Clock::Graphics) {
            vals.push(("clock_gr", "Core clock", "MHz", c as f64));
        }
        if let Ok(c) = dev.clock_info(Clock::Memory) {
            vals.push(("clock_mem", "Memory clock", "MHz", c as f64));
        }
        if let Ok(f) = dev.fan_speed(0) {
            vals.push(("fan", "Fan", "%", f as f64));
        }
        if let Ok(e) = dev.encoder_utilization() {
            vals.push(("enc", "Encoder", "%", e.utilization as f64));
        }
        if let Ok(d) = dev.decoder_utilization() {
            vals.push(("dec", "Decoder", "%", d.utilization as f64));
        }
        if let Ok(c) = dev.clock_info(Clock::SM) {
            vals.push(("clock_sm", "SM clock", "MHz", c as f64));
        }
        if let Ok(c) = dev.clock_info(Clock::Video) {
            vals.push(("clock_video", "Video clock", "MHz", c as f64));
        }
        if let Ok(s) = dev.encoder_stats() {
            vals.push((
                "enc_sessions",
                "Encoder sessions",
                "count",
                s.session_count as f64,
            ));
            if s.session_count > 0 {
                vals.push(("enc_fps", "Encoder FPS", "/s", s.average_fps as f64));
                vals.push((
                    "enc_latency",
                    "Encoder latency",
                    "ms",
                    s.average_latency as f64 / 1000.0,
                ));
            }
        }
        if let Ok(v) = dev.pcie_throughput(PcieUtilCounter::Receive) {
            vals.push(("pcie_rx", "PCIe to GPU", "B/s", v as f64 * 1024.0));
        }
        if let Ok(v) = dev.pcie_throughput(PcieUtilCounter::Send) {
            vals.push(("pcie_tx", "PCIe from GPU", "B/s", v as f64 * 1024.0));
        }
        if let Ok(p) = dev.enforced_power_limit() {
            vals.push(("power_limit", "Power limit", "W", p as f64 / 1000.0));
        }
        let slug = dev_slug(&gpu.addr);
        let pstate = dev.performance_state().ok().map(|p| format!("{p:?}"));
        if let (Ok(g), Ok(w)) = (dev.current_pcie_link_gen(), dev.current_pcie_link_width()) {
            out.info(format!("{slug}.pcie_link"), format!("Gen{g} x{w}"));
        }
        if let Ok(r) = dev.current_throttle_reasons() {
            out.info(format!("{slug}.throttle"), throttle_text(r));
        }

        // Processes using the GPU, with per-process utilization when supported.
        let mut procs: std::collections::BTreeMap<u32, (&str, Option<u64>)> = Default::default();
        for (kind, list) in [
            (
                "compute",
                dev.running_compute_processes().unwrap_or_default(),
            ),
            (
                "graphics",
                dev.running_graphics_processes().unwrap_or_default(),
            ),
        ] {
            for p in list {
                let mem = match p.used_gpu_memory {
                    UsedGpuMemory::Used(b) => Some(b),
                    UsedGpuMemory::Unavailable => None,
                };
                procs
                    .entry(p.pid)
                    .and_modify(|e| {
                        if e.0 != kind {
                            e.0 = "compute+graphics";
                        }
                        e.1 = e.1.max(mem);
                    })
                    .or_insert((kind, mem));
            }
        }
        let last = self.proc_ts.get(&gpu.addr).copied();
        let mut util: HashMap<u32, (u64, u32, u32, u32, u32)> = HashMap::new();
        if let Ok(samples) = dev.process_utilization_stats(last) {
            let mut newest = last.unwrap_or(0);
            for s in samples {
                newest = newest.max(s.timestamp);
                let e = util.entry(s.pid).or_insert((0, 0, 0, 0, 0));
                if s.timestamp >= e.0 {
                    *e = (s.timestamp, s.sm_util, s.mem_util, s.enc_util, s.dec_util);
                }
            }
            self.proc_ts.insert(gpu.addr.clone(), newest);
        }
        let mut list: Vec<Value> = procs
            .iter()
            .map(|(pid, (kind, mem))| {
                let name = read_opt(&self.roots.proc(&format!("{pid}/comm"))).unwrap_or_else(|| format!("pid {pid}"));
                let u = util.get(pid).copied().unwrap_or_default();
                json!({"pid": pid, "name": name, "kind": kind, "mem": mem, "sm": u.1, "mem_util": u.2, "enc": u.3, "dec": u.4})
            })
            .collect();
        list.sort_by(|a, b| {
            b["sm"]
                .as_u64()
                .cmp(&a["sm"].as_u64())
                .then(b["mem"].as_u64().cmp(&a["mem"].as_u64()))
        });
        vals.push(("procs", "Processes on GPU", "count", list.len() as f64));
        let mut extra = match out.extra.take() {
            Some(Value::Object(m)) => m,
            _ => serde_json::Map::new(),
        };
        extra.insert(slug.clone(), Value::Array(list));
        out.extra = Some(Value::Object(extra));

        if !self.facts.contains_key(&gpu.addr) {
            let mut f: Vec<(String, String)> = Vec::new();
            if let Ok(n) = dev.num_cores() {
                f.push(("cuda_cores".into(), n.to_string()));
            }
            if let Ok(a) = dev.architecture() {
                f.push(("architecture".into(), format!("{a:?}")));
            }
            if let Ok(c) = dev.cuda_compute_capability() {
                f.push((
                    "compute_capability".into(),
                    format!("{}.{}", c.major, c.minor),
                ));
            }
            if let (Ok(g), Ok(w)) = (dev.max_pcie_link_gen(), dev.max_pcie_link_width()) {
                f.push(("pcie_max".into(), format!("Gen{g} x{w}")));
            }
            if let Ok(b) = dev.memory_bus_width() {
                f.push(("memory_bus".into(), format!("{b}-bit")));
            }
            if let Ok(c) = dev.max_clock_info(Clock::Graphics) {
                f.push(("max_clock".into(), format!("{c} MHz")));
            }
            if let Ok(v) = dev.vbios_version() {
                f.push(("vbios".into(), v));
            }
            if let Ok(v) = nvml.sys_driver_version() {
                f.push(("driver".into(), v));
            }
            if let Ok(v) = nvml.sys_cuda_driver_version() {
                f.push(("cuda".into(), format!("{}.{}", v / 1000, (v % 1000) / 10)));
            }
            self.facts.insert(gpu.addr.clone(), f);
        }
        for (k, v) in self.facts.get(&gpu.addr).cloned().unwrap_or_default() {
            out.info(format!("{slug}.{k}"), v);
        }

        if let Some(n) = name {
            self.names.insert(gpu.addr.clone(), n);
        }
        for (metric, label, unit, v) in vals {
            let d = self.desc(gpu, metric, label, unit);
            out.push(&d, v);
        }
        if let Some(p) = pstate {
            out.info(format!("{slug}.pstate"), pstate_name(&p));
        }
        Ok(())
    }

    fn query_smi(
        &mut self,
        path: &Path,
        gpu: &PciGpu,
        ctx: &Ctx,
        out: &mut Out,
    ) -> std::io::Result<()> {
        // nvidia-smi is expensive; never run it more often than every 2 s.
        let fresh = self
            .last_smi
            .as_ref()
            .is_some_and(|(t, _)| ctx.now.duration_since(*t) < Duration::from_millis(1900));
        if !fresh {
            let s = run_with_timeout(
                path,
                &[
                    &format!("--query-gpu={SMI_FIELDS}"),
                    "--format=csv,noheader,nounits",
                ],
                SMI_TIMEOUT,
            )?;
            self.last_smi = Some((ctx.now, parse_smi(&s)));
        }
        let Some(row) = self
            .last_smi
            .as_ref()
            .and_then(|(_, rows)| rows.iter().find(|r| r.addr == gpu.addr).cloned())
        else {
            return Ok(());
        };
        if !row.name.is_empty() {
            self.names.insert(gpu.addr.clone(), row.name.clone());
        }
        let meta: [(&str, &str, &'static str, f64); 9] = [
            ("util", "GPU load", "%", 1.0),
            ("mem_util", "Memory controller", "%", 1.0),
            ("vram_used", "VRAM used", "B", 1048576.0),
            ("vram_total", "VRAM total", "B", 1048576.0),
            ("temp", "Temperature", "C", 1.0),
            ("power", "Power", "W", 1.0),
            ("clock_gr", "Core clock", "MHz", 1.0),
            ("clock_mem", "Memory clock", "MHz", 1.0),
            ("fan", "Fan", "%", 1.0),
        ];
        for (i, (metric, label, unit, scale)) in meta.iter().enumerate() {
            if let Some(v) = row.vals[i] {
                let d = self.desc(gpu, metric, label, unit);
                out.push(&d, v * scale);
            }
        }
        if let Some(p) = row.pstate {
            out.info(format!("{}.pstate", dev_slug(&gpu.addr)), p);
        }
        Ok(())
    }
}

/// Active clock limiters in plain words ("none" when not limited).
fn throttle_text(r: ThrottleReasons) -> String {
    let mut v = Vec::new();
    for (flag, word) in [
        (ThrottleReasons::SW_POWER_CAP, "power limit"),
        (ThrottleReasons::HW_SLOWDOWN, "hardware slowdown"),
        (ThrottleReasons::HW_POWER_BRAKE_SLOWDOWN, "power brake"),
        (ThrottleReasons::SW_THERMAL_SLOWDOWN, "thermal (driver)"),
        (ThrottleReasons::HW_THERMAL_SLOWDOWN, "thermal (hardware)"),
        (
            ThrottleReasons::APPLICATIONS_CLOCKS_SETTING,
            "application clocks",
        ),
        (ThrottleReasons::SYNC_BOOST, "sync boost"),
        (ThrottleReasons::DISPLAY_CLOCK_SETTING, "display clock"),
    ] {
        if r.contains(flag) {
            v.push(word);
        }
    }
    if v.is_empty() {
        if r.contains(ThrottleReasons::GPU_IDLE) {
            "idle".into()
        } else {
            "none".into()
        }
    } else {
        v.join(", ")
    }
}

fn pstate_name(debug: &str) -> String {
    let n = match debug {
        "Zero" => "0",
        "One" => "1",
        "Two" => "2",
        "Three" => "3",
        "Four" => "4",
        "Five" => "5",
        "Six" => "6",
        "Seven" => "7",
        "Eight" => "8",
        "Nine" => "9",
        "Ten" => "10",
        "Eleven" => "11",
        "Twelve" => "12",
        "Thirteen" => "13",
        "Fourteen" => "14",
        "Fifteen" => "15",
        _ => return debug.to_string(),
    };
    format!("P{n}")
}

impl Collector for GpuNvidia {
    fn collect(&mut self, ctx: &Ctx, out: &mut Out) -> anyhow::Result<()> {
        if self
            .last_scan
            .is_none_or(|t| ctx.now.duration_since(t) > Duration::from_secs(60))
        {
            self.gpus = scan_pci(&self.roots);
            self.last_scan = Some(ctx.now);
        }
        if self.gpus.is_empty() {
            out.health = Health::unavailable("no NVIDIA GPU found");
            return Ok(());
        }
        if self
            .gpus
            .iter()
            .all(|g| g.driver.as_deref() != Some("nvidia"))
        {
            out.health = Health::unavailable("NVIDIA GPU present but the proprietary nvidia driver is not bound")
                .hint("install/load the NVIDIA driver (nouveau GPUs are shown by the AMD/Intel GPU collector)");
            return Ok(());
        }

        let gpus = self.gpus.clone();
        let mut awake = Vec::new();
        for g in &gpus {
            let sleeping = runtime_suspended(&g.dir);
            let d = self.desc(g, "active", "Powered on", "bool");
            out.push(&d, if sleeping { 0.0 } else { 1.0 });
            out.info(
                format!("{}.state", dev_slug(&g.addr)),
                if sleeping { "sleeping" } else { "active" },
            );
            if !sleeping && g.driver.as_deref() == Some("nvidia") {
                awake.push(g.clone());
            }
        }
        if awake.is_empty() {
            out.health = Health::ok("runtime PM (GPU sleeping, not queried)");
            return Ok(());
        }

        let retry_due = self.retry_at.is_some_and(|t| ctx.now >= t);
        if matches!(self.backend, Backend::Uninit)
            || (retry_due && !matches!(self.backend, Backend::Nvml(_)))
        {
            self.init_backend();
        }

        let mut err: Option<String> = None;
        for g in &awake {
            match &self.backend {
                Backend::Nvml(_) => {
                    if let Err(e) = self.query_nvml(g, out) {
                        err = Some(format!("NVML: {e}"));
                        if matches!(
                            e,
                            NvmlError::GpuLost
                                | NvmlError::Uninitialized
                                | NvmlError::DriverNotLoaded
                                | NvmlError::LibRmVersionMismatch
                        ) {
                            self.backend = Backend::Uninit;
                            break;
                        }
                    }
                }
                Backend::Smi(p) => {
                    let p = p.clone();
                    if let Err(e) = self.query_smi(&p, g, ctx, out) {
                        err = Some(format!("nvidia-smi: {e}"));
                    }
                }
                Backend::None | Backend::Uninit => {}
            }
        }

        out.health = match (&self.backend, &err) {
            (Backend::Nvml(_), None) => Health::ok("NVML"),
            (Backend::Nvml(_), Some(e)) | (Backend::Uninit, Some(e)) => {
                Health::degraded("NVML", e.clone())
            }
            (Backend::Smi(_), None) => Health::degraded(
                "nvidia-smi",
                self.note
                    .clone()
                    .unwrap_or_else(|| "NVML unavailable".into()),
            ),
            (Backend::Smi(_), Some(e)) => Health::degraded("nvidia-smi", e.clone()),
            (Backend::None, _) | (Backend::Uninit, None) => Health::unavailable(
                self.note
                    .clone()
                    .unwrap_or_else(|| "no NVIDIA backend".into()),
            ),
        };
        if let Some(h) = &self.hint {
            out.health.hint = Some(h.clone());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smi_parse() {
        let s = "00000000:01:00.0, NVIDIA GeForce RTX 5070 Laptop GPU, 16, 9, 740, 8151, 58, 16.14, 510, 405, [N/A], P8\n";
        let r = parse_smi(s);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].addr, "0000:01:00.0");
        assert_eq!(r[0].vals[0], Some(16.0));
        assert_eq!(r[0].vals[8], None);
        assert_eq!(r[0].pstate.as_deref(), Some("P8"));
    }

    #[test]
    fn timeout_kills_child() {
        let t0 = Instant::now();
        let r = run_with_timeout(Path::new("/bin/sleep"), &["5"], Duration::from_millis(200));
        assert!(r.is_err());
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn sleeping_gpu_is_not_queried() {
        let roots = Roots::fixture("laptop");
        let mut g = GpuNvidia::new(roots);
        let settings = crate::settings::Settings::default();
        let ctx = Ctx {
            now: Instant::now(),
            wall_ms: 0,
            interval: Duration::from_secs(2),
            settings: &settings,
        };
        let mut out = Out::new();
        g.collect(&ctx, &mut out).unwrap();
        assert!(
            matches!(g.backend, Backend::Uninit),
            "backend must not be initialised"
        );
        let active = out
            .points
            .iter()
            .find(|(d, _)| d.metric == "active")
            .unwrap();
        assert_eq!(active.1, 0.0);
    }
}
