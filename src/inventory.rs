//! Hardware inventory: the physical devices found on this machine, stored in
//! the database (`meta` row `hardware`) and listed in Settings → Hardware.
//!
//! Detection is dynamic. The machine, CPU and memory come from DMI and
//! /proc; devices come from what the collectors discover: GPUs, disks,
//! physical network adapters, batteries and power supplies, sensor chips.
//! Card titles on the page stay live; the inventory is the stored record.
//!
//! * Every 60 s new devices are added and each entry is marked present or
//!   absent. Stored names and details are kept.
//! * A rescan rebuilds the device-discovering collectors, clears the PCI
//!   name cache and re-reads the host facts, then replaces the list with
//!   what was detected. An entry that was not found is removed only when its
//!   collector completed enough ticks after the rescan began; otherwise it
//!   stays, marked absent.

use crate::collectors::REDISCOVER;
use crate::hub::{GroupSeen, Hub};
use crate::model::State;
use crate::settings::Settings;
use crate::storage::Storage;
use crate::util::sysfs::Roots;
use crate::util::time::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::watch;

pub const FORMAT: u32 = 1;
const META_KEY: &str = "hardware";
const FIRST_SYNC: Duration = Duration::from_secs(15);
const SYNC_EVERY: Duration = Duration::from_secs(60);
/// A device whose series were updated this recently counts as present.
const PRESENT_WINDOW_MS: i64 = 120_000;
/// `last_seen` is written at most this often when nothing else changed.
const LAST_SEEN_WRITE_MS: i64 = 3_600_000;
const RESCAN_SPACING_MS: i64 = 10_000;
const RESCAN_MIN_WAIT_MS: u64 = 4_000;
const RESCAN_MAX_WAIT_MS: u64 = 30_000;
/// Facts that change while running; they are not stored.
const DYNAMIC_FACTS: &[&str] = &[
    "state",
    "status",
    "throttle",
    "pstate",
    "pcie_link",
    "speed_mbps",
];
const KIND_ORDER: &[&str] = &[
    "machine", "cpu", "memory", "gpu", "disk", "network", "power", "sensor",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Item {
    pub key: String,
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub details: BTreeMap<String, String>,
    pub first_seen: i64,
    pub last_seen: i64,
    pub present: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Inventory {
    pub format: u32,
    pub scanned_ms: i64,
    pub items: Vec<Item>,
}

/// A device found by one detection pass.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub key: String,
    pub kind: &'static str,
    pub name: String,
    pub details: BTreeMap<String, String>,
}

/// Inventory kind of a collector's device groups.
fn kind_of(collector: &str) -> Option<&'static str> {
    match collector {
        "gpu_nvidia" | "gpu_drm" => Some("gpu"),
        "disk" => Some("disk"),
        "network" => Some("network"),
        "battery" => Some("power"),
        "sensors" | "thermal" => Some("sensor"),
        _ => None,
    }
}

/// Collector that owns a device entry (`None` for host entries).
fn collector_of(key: &str) -> Option<&str> {
    key.split_once(':').map(|(c, _)| c)
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
}

/// Machine, CPU and memory from the host facts.
pub fn host_items(host: &Value, cpu_info: &BTreeMap<String, String>) -> Vec<Found> {
    let s = |k: &str| {
        host.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let mut out = Vec::new();
    let (vendor, product) = (s("vendor"), s("product"));
    let name = match (&vendor, &product) {
        (Some(v), Some(p)) if !p.to_lowercase().starts_with(&v.to_lowercase()) => {
            Some(format!("{v} {p}"))
        }
        (_, Some(p)) => Some(p.clone()),
        (Some(v), None) => Some(format!("{v} computer")),
        (None, None) => None,
    };
    if let Some(name) = name {
        let mut d = BTreeMap::new();
        for (k, v) in [
            ("vendor", vendor),
            ("model", product),
            ("board", s("board")),
            ("firmware", s("bios")),
        ] {
            if let Some(v) = v {
                d.insert(k.to_string(), v);
            }
        }
        out.push(Found {
            key: "machine".into(),
            kind: "machine",
            name,
            details: d,
        });
    }
    if let Some(cpu) = s("cpu") {
        let mut d = BTreeMap::new();
        if let Some(c) = cpu_info.get("cores") {
            d.insert("cores".to_string(), c.clone());
        }
        if let Some(t) = host
            .get("threads")
            .and_then(Value::as_u64)
            .filter(|t| *t > 0)
        {
            d.insert("threads".to_string(), t.to_string());
        }
        if let Some(a) = s("arch") {
            d.insert("architecture".to_string(), a);
        }
        out.push(Found {
            key: "cpu".into(),
            kind: "cpu",
            name: cpu,
            details: d,
        });
    }
    if let Some(m) = host.get("mem_total").and_then(Value::as_u64) {
        out.push(Found {
            key: "memory".into(),
            kind: "memory",
            name: "System memory".into(),
            details: [("usable".to_string(), gib(m))].into(),
        });
    }
    out
}

/// Devices from the live catalog whose series were updated at or after
/// `since_ms`. Network interfaces count only when they are physical.
pub fn device_items(
    groups: &[GroupSeen],
    info: &dyn Fn(&str) -> BTreeMap<String, String>,
    physical_net: &dyn Fn(&str) -> bool,
    since_ms: i64,
) -> Vec<Found> {
    let mut facts: HashMap<&str, BTreeMap<String, String>> = HashMap::new();
    let mut out = Vec::new();
    for g in groups {
        let Some(kind) = kind_of(g.collector) else {
            continue;
        };
        if g.last_ms < since_ms || (kind == "network" && !physical_net(&g.group)) {
            continue;
        }
        let all = facts
            .entry(g.collector)
            .or_insert_with(|| info(g.collector));
        let prefix = format!("{}.", g.group);
        let details = all
            .iter()
            .filter_map(|(k, v)| {
                let fact = k.strip_prefix(&prefix)?;
                let keep = !fact.contains('.')
                    && !DYNAMIC_FACTS.contains(&fact)
                    && !fact.contains("serial");
                keep.then(|| (fact.to_string(), v.clone()))
            })
            .collect();
        out.push(Found {
            key: format!("{}:{}", g.collector, g.group),
            kind,
            name: g.label.clone(),
            details,
        });
    }
    out
}

fn sort_items(items: &mut [Item]) {
    let rank = |k: &str| KIND_ORDER.iter().position(|x| *x == k).unwrap_or(99);
    items.sort_by(|a, b| {
        rank(&a.kind)
            .cmp(&rank(&b.kind))
            .then_with(|| crate::util::sysfs::natural_cmp(&a.name, &b.name))
    });
}

/// Periodic update: add new devices, mark presence, keep stored names.
/// Returns true when something other than `last_seen` changed.
pub fn merge(inv: &mut Inventory, found: Vec<Found>, now: i64) -> bool {
    let mut changed = false;
    let seen: HashSet<String> = found.iter().map(|f| f.key.clone()).collect();
    for f in found {
        match inv.items.iter_mut().find(|i| i.key == f.key) {
            Some(i) => {
                if !i.present {
                    i.present = true;
                    changed = true;
                }
                i.last_seen = now;
                for (k, v) in f.details {
                    if let std::collections::btree_map::Entry::Vacant(e) = i.details.entry(k) {
                        e.insert(v);
                        changed = true;
                    }
                }
            }
            None => {
                inv.items.push(Item {
                    key: f.key,
                    kind: f.kind.to_string(),
                    name: f.name,
                    details: f.details,
                    first_seen: now,
                    last_seen: now,
                    present: true,
                });
                changed = true;
            }
        }
    }
    for i in &mut inv.items {
        if i.present && !seen.contains(&i.key) {
            i.present = false;
            changed = true;
        }
    }
    if changed {
        sort_items(&mut inv.items);
    }
    changed
}

/// Rescan result: detected entries replace stored ones (fresh names, details
/// merged with the stored ones); entries not found are dropped when their
/// collector is in `confirmed`, otherwise kept with presence from
/// `recent` (keys with recently updated series).
pub fn replace(
    old: &Inventory,
    found: Vec<Found>,
    confirmed: &HashSet<&str>,
    recent: &HashSet<String>,
    now: i64,
) -> Inventory {
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for f in found {
        let prev = old.items.iter().find(|i| i.key == f.key);
        let mut details = prev.map(|p| p.details.clone()).unwrap_or_default();
        details.extend(f.details);
        seen.insert(f.key.clone());
        items.push(Item {
            first_seen: prev.map_or(now, |p| p.first_seen),
            key: f.key,
            kind: f.kind.to_string(),
            name: f.name,
            details,
            last_seen: now,
            present: true,
        });
    }
    for i in &old.items {
        if seen.contains(&i.key) {
            continue;
        }
        match collector_of(&i.key) {
            Some(c) if !confirmed.contains(c) => {
                let mut kept = i.clone();
                kept.present = recent.contains(&i.key);
                items.push(kept);
            }
            _ => {}
        }
    }
    sort_items(&mut items);
    Inventory {
        format: FORMAT,
        scanned_ms: now,
        items,
    }
}

#[derive(Debug, PartialEq)]
pub enum RescanError {
    Busy,
    TooSoon(i64),
}

pub struct Hardware {
    hub: Arc<Hub>,
    roots: Roots,
    storage: Option<Arc<Storage>>,
    settings: watch::Receiver<Arc<Settings>>,
    rescan_tx: watch::Sender<u64>,
    inv: Mutex<Inventory>,
    host: Mutex<Arc<Value>>,
    scanning: AtomicBool,
    last_rescan_ms: AtomicI64,
    last_write_ms: AtomicI64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Hardware {
    pub fn new(
        hub: Arc<Hub>,
        roots: Roots,
        storage: Option<Arc<Storage>>,
        settings: watch::Receiver<Arc<Settings>>,
        rescan_tx: watch::Sender<u64>,
        stored: Option<&str>,
        host: Value,
    ) -> Self {
        let inv = stored
            .and_then(|s| serde_json::from_str::<Inventory>(s).ok())
            .filter(|i| i.format == FORMAT)
            .unwrap_or_default();
        Self {
            hub,
            roots,
            storage,
            settings,
            rescan_tx,
            inv: Mutex::new(inv),
            host: Mutex::new(Arc::new(host)),
            scanning: AtomicBool::new(false),
            last_rescan_ms: AtomicI64::new(0),
            last_write_ms: AtomicI64::new(0),
        }
    }

    /// Host facts for the page header (refreshed by a rescan).
    pub fn host(&self) -> Arc<Value> {
        lock(&self.host).clone()
    }

    pub fn payload(&self, reason: &str) -> Value {
        let inv = lock(&self.inv).clone();
        json!({
            "reason": reason,
            "format": inv.format,
            "scanned_ms": inv.scanned_ms,
            "scanning": self.scanning.load(Ordering::Relaxed),
            "last_rescan_ms": self.last_rescan_ms.load(Ordering::Relaxed),
            "host": *self.host(),
            "items": inv.items,
        })
    }

    fn save(&self, inv: &Inventory) {
        if let (Some(st), Ok(j)) = (&self.storage, serde_json::to_string(inv)) {
            st.save_meta(META_KEY, j);
        }
        self.last_write_ms.store(now_ms(), Ordering::Relaxed);
    }

    fn detect(&self, since_ms: i64) -> Vec<Found> {
        let host = self.host();
        let mut found = host_items(&host, &self.hub.info_of("cpu"));
        let groups = self.hub.groups();
        let roots = self.roots.clone();
        let hub = self.hub.clone();
        found.extend(device_items(
            &groups,
            &|c| hub.info_of(c),
            &|iface| {
                !iface.contains('/') && roots.sys(&format!("class/net/{iface}/device")).exists()
            },
            since_ms,
        ));
        found
    }

    /// Periodic update (first run after start-up builds the initial list).
    pub fn sync(&self) {
        if self.scanning.load(Ordering::Relaxed) {
            return;
        }
        let now = now_ms();
        let found = self.detect(now - PRESENT_WINDOW_MS);
        let (changed, reason, snapshot) = {
            let mut inv = lock(&self.inv);
            if inv.format != FORMAT || inv.scanned_ms == 0 {
                *inv = replace(
                    &Inventory::default(),
                    found,
                    &HashSet::new(),
                    &HashSet::new(),
                    now,
                );
                (true, "detected", inv.clone())
            } else {
                let c = merge(&mut inv, found, now);
                (c, "updated", inv.clone())
            }
        };
        let stale = now - self.last_write_ms.load(Ordering::Relaxed) >= LAST_SEEN_WRITE_MS;
        if changed || stale {
            self.save(&snapshot);
        }
        if changed {
            self.hub.send("hardware", self.payload(reason));
        }
    }

    /// Start a rescan in the background. Returns the expected duration.
    pub fn start_rescan(self: &Arc<Self>) -> Result<u64, RescanError> {
        let since = now_ms() - self.last_rescan_ms.load(Ordering::Relaxed);
        if since < RESCAN_SPACING_MS {
            return Err(RescanError::TooSoon(RESCAN_SPACING_MS - since));
        }
        if self
            .scanning
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return Err(RescanError::Busy);
        }
        let settings = self.settings.borrow().clone();
        let wait_ms = REDISCOVER
            .iter()
            .map(|n| settings.collector(n))
            .filter(|c| c.enabled)
            .map(|c| c.interval_ms * 2 + 1000)
            .max()
            .unwrap_or(0)
            .clamp(RESCAN_MIN_WAIT_MS, RESCAN_MAX_WAIT_MS);
        let me = self.clone();
        tokio::spawn(async move {
            me.rescan(Duration::from_millis(wait_ms)).await;
        });
        Ok(wait_ms)
    }

    async fn rescan(self: Arc<Self>, max_wait: Duration) {
        let t0 = now_ms();
        tracing::info!("hardware rescan started");
        let ticks0: HashMap<&'static str, u64> = self
            .hub
            .statuses()
            .into_iter()
            .map(|s| (s.name, s.ticks))
            .collect();
        self.hub.send("hardware", self.payload("scanning"));
        crate::util::pciids::clear_cache();
        let roots = self.roots.clone();
        if let Ok(h) = tokio::task::spawn_blocking(move || crate::hostinfo::collect(&roots)).await {
            *lock(&self.host) = Arc::new(h);
        }
        self.rescan_tx.send_modify(|g| *g = g.wrapping_add(1));

        // A collector is confirmed when it completed two ticks after the
        // rescan began (rates need two), or one tick that found no source.
        let settings = self.settings.borrow().clone();
        let enabled: Vec<&'static str> = REDISCOVER
            .iter()
            .copied()
            .filter(|n| settings.collector(n).enabled)
            .collect();
        let confirmed = || -> HashSet<&'static str> {
            self.hub
                .statuses()
                .into_iter()
                .filter(|s| enabled.contains(&s.name))
                .filter(|s| {
                    let done = s.ticks.saturating_sub(*ticks0.get(s.name).unwrap_or(&0));
                    done >= 2 || (done >= 1 && s.state == State::Unavailable)
                })
                .map(|s| s.name)
                .collect()
        };
        let deadline = tokio::time::Instant::now() + max_wait;
        let mut ok = confirmed();
        while ok.len() < enabled.len() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(250)).await;
            ok = confirmed();
        }

        let now = now_ms();
        let found = self.detect(t0);
        let recent: HashSet<String> = self
            .hub
            .groups()
            .into_iter()
            .filter(|g| g.last_ms >= now - PRESENT_WINDOW_MS)
            .map(|g| format!("{}:{}", g.collector, g.group))
            .collect();
        let snapshot = {
            let mut inv = lock(&self.inv);
            let new = replace(&inv, found, &ok, &recent, now);
            *inv = new;
            inv.clone()
        };
        self.save(&snapshot);
        self.last_rescan_ms.store(now_ms(), Ordering::Relaxed);
        self.scanning.store(false, Ordering::Release);
        let missing: Vec<&str> = enabled
            .iter()
            .copied()
            .filter(|n| !ok.contains(n))
            .collect();
        if missing.is_empty() {
            tracing::info!(
                "hardware rescan finished: {} entries in {} ms",
                snapshot.items.len(),
                now - t0
            );
        } else {
            tracing::info!(
                "hardware rescan finished: {} entries; not confirmed in time (entries kept): {}",
                snapshot.items.len(),
                missing.join(", ")
            );
        }
        self.hub.send("hardware", self.payload("rescanned"));
    }
}

/// Background task: first update shortly after start-up, then every minute.
pub async fn run(hw: Arc<Hardware>, mut shutdown: watch::Receiver<bool>) {
    let mut wait = FIRST_SYNC;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shutdown.changed() => break,
        }
        wait = SYNC_EVERY;
        let h = hw.clone();
        let _ = tokio::task::spawn_blocking(move || h.sync()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(c: &'static str, g: &str, label: &str, last_ms: i64) -> GroupSeen {
        GroupSeen {
            collector: c,
            group: g.into(),
            label: label.into(),
            last_ms,
        }
    }

    #[test]
    fn host_entries() {
        let host = json!({"vendor": "Example Inc.", "product": "Example Laptop 15", "bios": "1.2",
                          "cpu": "Example CPU 8-Core", "threads": 16, "arch": "x86_64", "mem_total": 17179869184u64});
        let info: BTreeMap<String, String> = [("cores".to_string(), "8".to_string())].into();
        let f = host_items(&host, &info);
        assert_eq!(f[0].name, "Example Inc. Example Laptop 15");
        assert_eq!(f[0].details["firmware"], "1.2");
        assert_eq!(f[1].details["cores"], "8");
        assert_eq!(f[1].details["threads"], "16");
        assert_eq!(f[2].details["usable"], "16.0 GiB");
        // Product names that already start with the vendor are not repeated.
        let f = host_items(
            &json!({"vendor": "HP", "product": "hp Example"}),
            &BTreeMap::new(),
        );
        assert_eq!(f[0].name, "hp Example");
        // A VM without DMI data has no machine entry.
        assert!(host_items(&json!({"cpu": "QEMU Virtual CPU"}), &BTreeMap::new())[0].kind == "cpu");
    }

    #[test]
    fn devices_and_facts() {
        let groups = vec![
            group("gpu_nvidia", "0000-01-00.0", "NVIDIA Example GPU", 100),
            group("network", "eth0", "eth0 (Ethernet)", 100),
            group("network", "vpn0", "vpn0 (virtual)", 100),
            group("cpu", "core0", "core 0", 100),
            group("disk", "old_disk", "Old disk (sdb)", 10),
        ];
        let info = |c: &str| -> BTreeMap<String, String> {
            if c == "gpu_nvidia" {
                [
                    ("0000-01-00.0.vbios", "1.0"),
                    ("0000-01-00.0.state", "active"),
                    ("0000-01-00.0.serial", "X"),
                    ("other.vbios", "2.0"),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
            } else {
                BTreeMap::new()
            }
        };
        let f = device_items(&groups, &info, &|i| i == "eth0", 50);
        let keys: Vec<&str> = f.iter().map(|x| x.key.as_str()).collect();
        assert_eq!(keys, vec!["gpu_nvidia:0000-01-00.0", "network:eth0"]);
        assert_eq!(f[0].details.len(), 1);
        assert_eq!(f[0].details["vbios"], "1.0");
    }

    fn found(key: &str, kind: &'static str, name: &str) -> Found {
        Found {
            key: key.into(),
            kind,
            name: name.into(),
            details: BTreeMap::new(),
        }
    }

    #[test]
    fn merge_keeps_names_and_marks_presence() {
        let mut inv = replace(
            &Inventory::default(),
            vec![
                found("cpu", "cpu", "CPU A"),
                found("disk:a", "disk", "Disk A (sda)"),
            ],
            &HashSet::new(),
            &HashSet::new(),
            1,
        );
        assert_eq!(inv.items.len(), 2);
        // The disk is gone, a USB disk appeared and the CPU name changed:
        // the stored CPU name is kept until a rescan.
        let changed = merge(
            &mut inv,
            vec![
                found("cpu", "cpu", "CPU B"),
                found("disk:b", "disk", "USB disk"),
            ],
            2,
        );
        assert!(changed);
        let get = |k: &str| inv.items.iter().find(|i| i.key == k).unwrap().clone();
        assert_eq!(get("cpu").name, "CPU A");
        assert!(!get("disk:a").present);
        assert!(get("disk:b").present);
        assert_eq!(get("disk:b").first_seen, 2);
        assert!(!merge(
            &mut inv,
            vec![
                found("cpu", "cpu", "CPU B"),
                found("disk:b", "disk", "USB disk")
            ],
            3
        ));
    }

    #[test]
    fn replace_drops_only_confirmed_absences() {
        let old = replace(
            &Inventory::default(),
            vec![
                found("cpu", "cpu", "CPU A"),
                found("disk:a", "disk", "Disk A"),
                found("gpu_nvidia:x", "gpu", "GPU X"),
            ],
            &HashSet::new(),
            &HashSet::new(),
            1,
        );
        // Disk collector confirmed (disk:a removed), NVIDIA not confirmed
        // (its GPU stays, marked absent), CPU renamed by the rescan.
        let confirmed: HashSet<&str> = ["disk"].into();
        let new = replace(
            &old,
            vec![found("cpu", "cpu", "CPU B")],
            &confirmed,
            &HashSet::new(),
            5,
        );
        let keys: Vec<&str> = new.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, vec!["cpu", "gpu_nvidia:x"]);
        assert_eq!(new.items[0].name, "CPU B");
        assert_eq!(new.items[0].first_seen, 1);
        assert!(!new.items[1].present);
        assert_eq!(new.scanned_ms, 5);
    }
}
