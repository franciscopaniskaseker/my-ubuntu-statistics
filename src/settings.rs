//! Runtime settings (changed from the UI, persisted in the database).
//!
//! Every value is clamped to a safe range; unknown fields are ignored and
//! missing fields take defaults, so a damaged settings document can never
//! prevent the service from starting.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Known collectors with their default interval (ms) and UI label.
pub const COLLECTORS: &[(&str, u64, &str)] = &[
    ("cpu", 1000, "CPU"),
    ("system", 2000, "System & load"),
    ("pressure", 2000, "Pressure (PSI)"),
    ("memory", 2000, "Memory"),
    ("disk", 3000, "Disk I/O"),
    ("filesystem", 30000, "Filesystems"),
    ("network", 3000, "Network"),
    ("sensors", 2000, "Sensors (hwmon)"),
    ("thermal", 5000, "Thermal zones"),
    ("power", 2000, "CPU power (RAPL)"),
    ("battery", 5000, "Battery & AC"),
    ("gpu_nvidia", 2000, "GPU (NVIDIA)"),
    ("gpu_drm", 2000, "GPU (AMD/Intel)"),
    ("processes", 3000, "Processes"),
];

pub const MIN_INTERVAL_MS: u64 = 250;
pub const MAX_INTERVAL_MS: u64 = 60_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub collectors: BTreeMap<String, CollectorCfg>,
    pub history: History,
    pub backups: Backups,
    pub display: Display,
    pub network: NetworkCfg,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CollectorCfg {
    pub enabled: bool,
    pub interval_ms: u64,
}

impl Default for CollectorCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_ms: 2000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct History {
    /// Width of the averaged buckets written to the raw tier (seconds).
    pub resolution_s: u64,
    pub raw_retention_hours: u64,
    pub minute_retention_days: u64,
    pub hour_retention_days: u64,
    pub max_db_mb: u64,
}

impl Default for History {
    fn default() -> Self {
        Self {
            resolution_s: 10,
            raw_retention_hours: 48,
            minute_retention_days: 14,
            hour_retention_days: 365,
            max_db_mb: 2048,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Backups {
    pub enabled: bool,
    pub keep_daily: u32,
    pub keep_weekly: u32,
    pub max_total_mb: u64,
}

impl Default for Backups {
    fn default() -> Self {
        Self {
            enabled: true,
            keep_daily: 7,
            keep_weekly: 4,
            max_total_mb: 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Display {
    pub live_window_s: u64,
    /// off | low | medium | high
    pub smoothing: String,
    /// auto | dark | light
    pub theme: String,
    /// C | F
    pub temp_unit: String,
}

impl Default for Display {
    fn default() -> Self {
        Self {
            live_window_s: 300,
            smoothing: "medium".into(),
            theme: "auto".into(),
            temp_unit: "C".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct NetworkCfg {
    /// Interface name globs that are not collected.
    pub exclude: Vec<String>,
}

impl Default for NetworkCfg {
    fn default() -> Self {
        Self {
            exclude: [
                "lo", "veth*", "docker*", "br-*", "virbr*", "vnet*", "cni*", "flannel*",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        let mut s = Self {
            collectors: BTreeMap::new(),
            history: History::default(),
            backups: Backups::default(),
            display: Display::default(),
            network: NetworkCfg::default(),
        };
        s.normalize();
        s
    }
}

fn clamp(v: u64, lo: u64, hi: u64) -> u64 {
    v.clamp(lo, hi)
}

impl Settings {
    /// Fill missing collectors, drop unknown ones and clamp every value.
    pub fn normalize(&mut self) {
        let mut cols = BTreeMap::new();
        for (name, default_ms, _) in COLLECTORS {
            let mut c = self.collectors.remove(*name).unwrap_or(CollectorCfg {
                enabled: true,
                interval_ms: *default_ms,
            });
            c.interval_ms = clamp(c.interval_ms, MIN_INTERVAL_MS, MAX_INTERVAL_MS);
            cols.insert(name.to_string(), c);
        }
        self.collectors = cols;

        let h = &mut self.history;
        h.resolution_s = clamp(h.resolution_s, 1, 300);
        h.raw_retention_hours = clamp(h.raw_retention_hours, 1, 24 * 30);
        h.minute_retention_days = clamp(h.minute_retention_days, 1, 365);
        h.hour_retention_days = clamp(h.hour_retention_days, 1, 3650);
        h.max_db_mb = clamp(h.max_db_mb, 64, 1024 * 1024);

        let b = &mut self.backups;
        b.keep_daily = b.keep_daily.clamp(1, 365);
        b.keep_weekly = b.keep_weekly.min(520);
        b.max_total_mb = clamp(b.max_total_mb, 16, 1024 * 1024);

        let d = &mut self.display;
        d.live_window_s = clamp(d.live_window_s, 60, 900);
        if !["off", "low", "medium", "high"].contains(&d.smoothing.as_str()) {
            d.smoothing = "medium".into();
        }
        if !["auto", "dark", "light"].contains(&d.theme.as_str()) {
            d.theme = "auto".into();
        }
        if !["C", "F"].contains(&d.temp_unit.as_str()) {
            d.temp_unit = "C".into();
        }

        self.network
            .exclude
            .retain(|p| !p.trim().is_empty() && p.len() <= 64);
        self.network.exclude.truncate(64);
    }

    pub fn collector(&self, name: &str) -> CollectorCfg {
        self.collectors.get(name).cloned().unwrap_or_default()
    }

    /// Parse a stored settings document, falling back to defaults.
    pub fn from_json_lenient(s: &str) -> (Self, Option<String>) {
        match serde_json::from_str::<Settings>(s) {
            Ok(mut v) => {
                v.normalize();
                (v, None)
            }
            Err(e) => (Settings::default(), Some(e.to_string())),
        }
    }

    /// Apply a partial update (JSON merge patch) and validate the result.
    pub fn apply_patch(&self, patch: &Value) -> Result<Settings, String> {
        if !patch.is_object() {
            return Err("settings update must be a JSON object".into());
        }
        let mut base = serde_json::to_value(self).map_err(|e| e.to_string())?;
        merge(&mut base, patch);
        let mut s: Settings = serde_json::from_value(base).map_err(|e| e.to_string())?;
        s.normalize();
        Ok(s)
    }
}

/// RFC 7386 JSON merge patch.
fn merge(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(t), Value::Object(p)) => {
            for (k, v) in p {
                if v.is_null() {
                    t.remove(k);
                } else {
                    merge(t.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
        }
        (t, p) => *t = p.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_cover_all_collectors() {
        let s = Settings::default();
        assert_eq!(s.collectors.len(), COLLECTORS.len());
        assert_eq!(s.collector("cpu").interval_ms, 1000);
    }

    #[test]
    fn patch_clamps_and_merges() {
        let s = Settings::default();
        let p = json!({"collectors": {"cpu": {"interval_ms": 5}}, "display": {"theme": "neon"}});
        let n = s.apply_patch(&p).unwrap();
        assert_eq!(n.collector("cpu").interval_ms, MIN_INTERVAL_MS);
        assert!(n.collector("cpu").enabled);
        assert_eq!(n.display.theme, "auto");
        assert_eq!(n.collector("disk").interval_ms, 3000);
    }

    #[test]
    fn lenient_parse() {
        let (s, err) = Settings::from_json_lenient("{not json");
        assert!(err.is_some());
        assert_eq!(s, Settings::default());
        let (s, err) = Settings::from_json_lenient(r#"{"history":{"resolution_s":0},"bogus":1}"#);
        assert!(err.is_none());
        assert_eq!(s.history.resolution_s, 1);
    }

    #[test]
    fn rejects_wrong_types() {
        let s = Settings::default();
        assert!(
            s.apply_patch(&json!({"history": {"resolution_s": "x"}}))
                .is_err()
        );
        assert!(s.apply_patch(&json!([1, 2])).is_err());
    }
}
