//! Shared data model: series descriptors, collector output and status.

use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// Describes one time series. Keys are stable across reboots.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SeriesDesc {
    /// Unique, stable key (e.g. `hwmon.k10temp.0000-00-18.3.temp1`).
    pub key: String,
    /// Collector that produces it (`cpu`, `disk`, `hwmon`, ...).
    pub collector: &'static str,
    /// Metric id within the group (`usage`, `read_bps`, `temp`, ...).
    pub metric: String,
    /// Group id (device, chip, interface); empty for global series.
    pub group: String,
    /// Human friendly group name.
    pub group_label: String,
    /// Human friendly series name.
    pub label: String,
    /// Unit: `%`, `C`, `rpm`, `W`, `V`, `A`, `MHz`, `B`, `B/s`, `/s`, `ms`, `s`, `count`, `Wh`, `bool`, `dBm`.
    pub unit: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Expected time between samples when it differs from the collector
    /// interval (slow sensors polled less often).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub period_ms: Option<u64>,
}

pub type Desc = Arc<SeriesDesc>;

impl SeriesDesc {
    pub fn new(
        collector: &'static str,
        key: impl Into<String>,
        metric: impl Into<String>,
        label: impl Into<String>,
        unit: &'static str,
    ) -> Self {
        Self {
            key: key.into(),
            collector,
            metric: metric.into(),
            group: String::new(),
            group_label: String::new(),
            label: label.into(),
            unit,
            crit: None,
            max: None,
            period_ms: None,
        }
    }

    pub fn group(mut self, id: impl Into<String>, label: impl Into<String>) -> Self {
        self.group = id.into();
        self.group_label = label.into();
        self
    }

    pub fn limits(mut self, max: Option<f64>, crit: Option<f64>) -> Self {
        self.max = max;
        self.crit = crit;
        self
    }
}

/// Per-collector cache so descriptors are allocated once per series.
#[derive(Debug, Default)]
pub struct DescCache {
    map: HashMap<String, Desc>,
}

impl DescCache {
    pub fn get(&mut self, key: &str, make: impl FnOnce() -> SeriesDesc) -> Desc {
        if let Some(d) = self.map.get(key) {
            return d.clone();
        }
        let d = Arc::new(make());
        self.map.insert(key.to_string(), d.clone());
        d
    }

    /// Replace a descriptor whose label changed (e.g. device renamed).
    pub fn put(&mut self, desc: SeriesDesc) -> Desc {
        let d = Arc::new(desc);
        self.map.insert(d.key.clone(), d.clone());
        d
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Starting,
    Ok,
    Degraded,
    Unavailable,
    Disabled,
    Stalled,
    Error,
}

/// Health reported by a collector for one tick.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Health {
    pub state: State,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Health {
    pub fn ok(source: impl Into<String>) -> Self {
        Self {
            state: State::Ok,
            source: source.into(),
            reason: None,
            hint: None,
        }
    }

    pub fn degraded(source: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            state: State::Degraded,
            source: source.into(),
            reason: Some(reason.into()),
            hint: None,
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            state: State::Unavailable,
            source: String::new(),
            reason: Some(reason.into()),
            hint: None,
        }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Output of one collector tick.
#[derive(Debug)]
pub struct Out {
    pub points: Vec<(Desc, f64)>,
    /// Text facts shown in the UI but not stored (governor, battery status...).
    pub info: BTreeMap<String, String>,
    /// Free-form JSON payload broadcast to clients (e.g. top applications).
    pub extra: Option<serde_json::Value>,
    /// Larger JSON kept in memory for on-demand API requests (not broadcast).
    pub detail: Option<serde_json::Value>,
    pub health: Health,
}

impl Out {
    pub fn new() -> Self {
        Self {
            points: Vec::with_capacity(64),
            info: BTreeMap::new(),
            extra: None,
            detail: None,
            health: Health::ok(""),
        }
    }

    /// Push a value; non-finite numbers are silently dropped.
    pub fn push(&mut self, desc: &Desc, value: f64) {
        if value.is_finite() {
            self.points.push((desc.clone(), value));
        }
    }

    pub fn info(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.info.insert(key.into(), value.into());
    }
}

impl Default for Out {
    fn default() -> Self {
        Self::new()
    }
}

/// Status of a collector as seen by the scheduler (exposed in /api/status).
#[derive(Debug, Clone, Serialize)]
pub struct CollectorStatus {
    pub name: &'static str,
    pub state: State,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    pub interval_ms: u64,
    pub series: usize,
    pub last_ok_ms: i64,
    pub last_duration_ms: f64,
    pub errors: u64,
    pub restarts: u64,
}

impl CollectorStatus {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            state: State::Starting,
            source: String::new(),
            reason: None,
            hint: None,
            interval_ms: 0,
            series: 0,
            last_ok_ms: 0,
            last_duration_ms: 0.0,
            errors: 0,
            restarts: 0,
        }
    }
}
