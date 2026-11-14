//! In-memory hub: latest values, bounded per-series rings for the live view,
//! accumulators for persistence and the SSE broadcast channel.

use crate::model::{CollectorStatus, Desc, Out, State};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::broadcast;

/// Hard cap on the number of live series (protects memory and the DB).
pub const MAX_SERIES: usize = 2000;
/// Live ring window (the largest selectable live window).
pub const RING_WINDOW_MS: i64 = 15 * 60 * 1000;
/// Maximum points kept per series in the ring.
pub const RING_MAX_POINTS: usize = 1800;
/// Series not updated for this long disappear from the live catalog.
pub const STALE_MS: i64 = 10 * 60 * 1000;

#[derive(Debug)]
pub struct SseMsg {
    pub event: &'static str,
    pub data: String,
}

struct Entry {
    desc: Desc,
    ring: VecDeque<(i64, f32)>,
    last_ms: i64,
    last: f64,
}

struct Acc {
    desc: Desc,
    sum: f64,
    n: u32,
}

#[derive(Default)]
struct Inner {
    series: HashMap<String, Entry>,
    catalog_version: u64,
    info: BTreeMap<&'static str, BTreeMap<String, String>>,
    processes: Option<Value>,
    extras: BTreeMap<&'static str, Value>,
    details: BTreeMap<&'static str, Arc<Value>>,
    pending: HashMap<String, Acc>,
    dropped_series: u64,
}

pub struct Hub {
    inner: Mutex<Inner>,
    status: Mutex<BTreeMap<&'static str, CollectorStatus>>,
    tx: broadcast::Sender<Arc<SseMsg>>,
    pub started_ms: i64,
    pub instance: String,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding the lock must not take the whole service down.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Round values so the JSON stays compact without losing useful precision.
pub fn round_value(v: f64) -> f64 {
    if v.abs() >= 1000.0 {
        v.round()
    } else if v.abs() >= 10.0 {
        (v * 100.0).round() / 100.0
    } else {
        (v * 1000.0).round() / 1000.0
    }
}

impl Hub {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        let started_ms = crate::util::time::now_ms();
        Self {
            inner: Mutex::new(Inner::default()),
            status: Mutex::new(BTreeMap::new()),
            tx,
            started_ms,
            instance: format!("{:x}-{:x}", started_ms, std::process::id()),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<SseMsg>> {
        self.tx.subscribe()
    }

    pub fn send(&self, event: &'static str, data: Value) {
        let _ = self.tx.send(Arc::new(SseMsg {
            event,
            data: data.to_string(),
        }));
    }

    /// Store a collector tick and broadcast it.
    pub fn publish(&self, collector: &'static str, ts_ms: i64, out: Out) {
        let mut values = Map::with_capacity(out.points.len());
        let mut catalog_changed = false;
        let mut info_changed = false;
        {
            let mut g = lock(&self.inner);
            let g = &mut *g;
            for (desc, v) in out.points {
                let entry = match g.series.get_mut(&desc.key) {
                    Some(e) => {
                        if !Arc::ptr_eq(&e.desc, &desc) && *e.desc != *desc {
                            e.desc = desc.clone();
                            catalog_changed = true;
                        }
                        e
                    }
                    None => {
                        if g.series.len() >= MAX_SERIES {
                            g.dropped_series += 1;
                            crate::warn_limited!(
                                "hub:max_series",
                                "series limit ({MAX_SERIES}) reached; ignoring new series such as {}",
                                desc.key
                            );
                            continue;
                        }
                        catalog_changed = true;
                        g.series.entry(desc.key.clone()).or_insert(Entry {
                            desc: desc.clone(),
                            ring: VecDeque::with_capacity(64),
                            last_ms: 0,
                            last: 0.0,
                        })
                    }
                };
                entry.ring.push_back((ts_ms, v as f32));
                let cutoff = ts_ms - RING_WINDOW_MS;
                while entry
                    .ring
                    .front()
                    .is_some_and(|(t, _)| *t < cutoff || entry.ring.len() > RING_MAX_POINTS)
                {
                    entry.ring.pop_front();
                }
                entry.last = v;
                entry.last_ms = ts_ms;

                let acc = g.pending.entry(desc.key.clone()).or_insert(Acc {
                    desc: desc.clone(),
                    sum: 0.0,
                    n: 0,
                });
                acc.desc = desc.clone();
                acc.sum += v;
                acc.n += 1;

                values.insert(desc.key.clone(), json!(round_value(v)));
            }
            if catalog_changed {
                g.catalog_version += 1;
            }
            if !out.info.is_empty() {
                let prev = g.info.get(collector);
                if prev != Some(&out.info) {
                    g.info.insert(collector, out.info.clone());
                    info_changed = true;
                }
            }
            if let Some(d) = &out.detail {
                g.details.insert(collector, Arc::new(d.clone()));
            }
            if let Some(extra) = &out.extra {
                if collector == "processes" {
                    g.processes = Some(extra.clone());
                } else {
                    g.extras.insert(collector, extra.clone());
                }
            }
        }

        if !values.is_empty() {
            self.send("sample", json!({"c": collector, "t": ts_ms, "v": values}));
        }
        if info_changed {
            self.send("info", json!({"c": collector, "info": out.info}));
        }
        if let Some(extra) = out.extra {
            if collector == "processes" {
                self.send("processes", extra);
            } else {
                self.send("extra", json!({"c": collector, "data": extra}));
            }
        }
        if catalog_changed {
            let v = lock(&self.inner).catalog_version;
            self.send("catalog", json!({"version": v}));
        }
    }

    /// Drain averaged values accumulated since the last call.
    pub fn take_pending(&self) -> Vec<(Desc, f64)> {
        let mut g = lock(&self.inner);
        g.pending
            .drain()
            .filter(|(_, a)| a.n > 0)
            .map(|(_, a)| (a.desc, a.sum / a.n as f64))
            .collect()
    }

    /// Remove series that have not been updated recently.
    pub fn prune_stale(&self, now_ms: i64) {
        let removed = {
            let mut g = lock(&self.inner);
            let before = g.series.len();
            g.series.retain(|_, e| now_ms - e.last_ms < STALE_MS);
            let removed = before - g.series.len();
            if removed > 0 {
                g.catalog_version += 1;
            }
            removed
        };
        if removed > 0 {
            let v = lock(&self.inner).catalog_version;
            self.send("catalog", json!({"version": v}));
        }
    }

    pub fn catalog(&self) -> (u64, Vec<Desc>) {
        let g = lock(&self.inner);
        let mut v: Vec<Desc> = g.series.values().map(|e| e.desc.clone()).collect();
        v.sort_by(|a, b| crate::util::sysfs::natural_cmp(&a.key, &b.key));
        (g.catalog_version, v)
    }

    pub fn desc(&self, key: &str) -> Option<Desc> {
        lock(&self.inner).series.get(key).map(|e| e.desc.clone())
    }

    pub fn latest(&self, key: &str) -> Option<(i64, f64)> {
        lock(&self.inner)
            .series
            .get(key)
            .map(|e| (e.last_ms, e.last))
    }

    pub fn snapshot(&self) -> Value {
        let g = lock(&self.inner);
        let mut series = Map::with_capacity(g.series.len());
        for (k, e) in &g.series {
            series.insert(k.clone(), json!([e.last_ms, round_value(e.last)]));
        }
        json!({
            "catalog_version": g.catalog_version,
            "series": series,
            "info": g.info,
            "processes": g.processes,
            "extra": g.extras,
        })
    }

    /// Ring contents for the given keys newer than `since_ms`.
    pub fn live(&self, keys: &[String], since_ms: i64) -> Map<String, Value> {
        let g = lock(&self.inner);
        let mut out = Map::new();
        for k in keys {
            if let Some(e) = g.series.get(k) {
                let (mut t, mut v) = (Vec::new(), Vec::new());
                for (ts, val) in e.ring.iter().filter(|(ts, _)| *ts >= since_ms) {
                    t.push(json!(ts));
                    v.push(json!(round_value(*val as f64)));
                }
                out.insert(k.clone(), json!({"t": t, "v": v}));
            }
        }
        out
    }

    /// Ring points for one key newer than `since_ms` (used to fill the gap
    /// between the last persisted bucket and now).
    pub fn ring_points(&self, key: &str, since_ms: i64) -> Vec<(i64, f64)> {
        let g = lock(&self.inner);
        g.series
            .get(key)
            .map(|e| {
                e.ring
                    .iter()
                    .filter(|(t, _)| *t > since_ms)
                    .map(|(t, v)| (*t, *v as f64))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Latest on-demand detail of a collector (e.g. every process).
    pub fn detail(&self, collector: &str) -> Option<Arc<Value>> {
        lock(&self.inner).details.get(collector).cloned()
    }

    pub fn dropped_series(&self) -> u64 {
        lock(&self.inner).dropped_series
    }

    pub fn update_status(&self, name: &'static str, f: impl FnOnce(&mut CollectorStatus)) {
        let changed = {
            let mut g = lock(&self.status);
            let st = g.entry(name).or_insert_with(|| CollectorStatus::new(name));
            let before = (st.state, st.reason.clone());
            f(st);
            before != (st.state, st.reason.clone())
        };
        if changed {
            let st = lock(&self.status).get(name).cloned();
            if let Some(st) = st {
                self.send("status", json!(st));
            }
        }
    }

    pub fn statuses(&self) -> Vec<CollectorStatus> {
        lock(&self.status).values().cloned().collect()
    }

    pub fn overall_state(&self) -> State {
        let g = lock(&self.status);
        if g.values()
            .any(|s| matches!(s.state, State::Error | State::Stalled))
        {
            State::Degraded
        } else {
            State::Ok
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SeriesDesc;

    #[test]
    fn publish_and_pending() {
        let hub = Hub::new();
        let d: Desc = Arc::new(SeriesDesc::new("cpu", "cpu.usage", "usage", "CPU", "%"));
        for (i, v) in [10.0, 20.0, 30.0].iter().enumerate() {
            let mut out = Out::new();
            out.push(&d, *v);
            hub.publish("cpu", 1000 * i as i64, out);
        }
        assert_eq!(hub.latest("cpu.usage"), Some((2000, 30.0)));
        let p = hub.take_pending();
        assert_eq!(p.len(), 1);
        assert!((p[0].1 - 20.0).abs() < 1e-9);
        assert!(hub.take_pending().is_empty());
        let (v, cat) = hub.catalog();
        assert_eq!(v, 1);
        assert_eq!(cat.len(), 1);
        hub.prune_stale(2000 + STALE_MS + 1);
        assert!(hub.catalog().1.is_empty());
    }
}
