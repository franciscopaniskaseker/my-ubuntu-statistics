//! Helpers to turn monotonically increasing counters into rates.
//!
//! Rates are only produced when the previous sample is recent enough. A gap
//! larger than `max_gap` (suspend/resume, stalled collector) or a counter that
//! went backwards (reset, device re-plug) yields `None` instead of a spike.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Tracks the time between ticks of a collector.
#[derive(Debug, Default)]
pub struct TickClock {
    last: Option<Instant>,
}

impl TickClock {
    /// Returns the elapsed seconds since the previous tick when it is usable
    /// for rate computation.
    pub fn tick(&mut self, now: Instant, interval: Duration) -> Option<f64> {
        let prev = self.last.replace(now);
        let prev = prev?;
        let dt = now.checked_duration_since(prev)?;
        if dt.is_zero() || dt > max_gap(interval) {
            return None;
        }
        Some(dt.as_secs_f64())
    }
}

/// Largest acceptable gap between two samples for a rate.
pub fn max_gap(interval: Duration) -> Duration {
    (interval * 3).max(Duration::from_secs(3))
}

/// Difference of a counter, `None` when it went backwards.
pub fn delta(prev: u64, cur: u64) -> Option<u64> {
    cur.checked_sub(prev)
}

/// Difference of a counter that wraps at `max` (e.g. RAPL energy).
pub fn delta_wrap(prev: u64, cur: u64, max: Option<u64>) -> Option<u64> {
    if cur >= prev {
        return Some(cur - prev);
    }
    match max {
        Some(m) if m >= prev => Some((m - prev).saturating_add(cur)),
        _ => None,
    }
}

/// Keyed counter tracker for things like per-interface byte counters.
#[derive(Debug, Default)]
pub struct Rates {
    prev: HashMap<String, (u64, Instant)>,
}

impl Rates {
    pub fn rate(&mut self, key: &str, value: u64, now: Instant, interval: Duration) -> Option<f64> {
        self.rate_wrap(key, value, now, interval, None)
    }

    pub fn rate_wrap(
        &mut self,
        key: &str,
        value: u64,
        now: Instant,
        interval: Duration,
        wrap: Option<u64>,
    ) -> Option<f64> {
        let prev = match self.prev.get_mut(key) {
            Some(slot) => std::mem::replace(slot, (value, now)),
            None => {
                self.prev.insert(key.to_string(), (value, now));
                return None;
            }
        };
        let dt = now.checked_duration_since(prev.1)?;
        if dt.is_zero() || dt > max_gap(interval) {
            return None;
        }
        let d = delta_wrap(prev.0, value, wrap)?;
        Some(d as f64 / dt.as_secs_f64())
    }

    /// Drop state for keys that were not seen recently.
    pub fn retain_recent(&mut self, now: Instant, keep: Duration) {
        self.prev
            .retain(|_, (_, t)| now.checked_duration_since(*t).is_none_or(|d| d < keep));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_and_reset() {
        assert_eq!(delta(10, 15), Some(5));
        assert_eq!(delta(15, 10), None);
        assert_eq!(delta_wrap(90, 10, Some(100)), Some(20));
        assert_eq!(delta_wrap(90, 10, None), None);
        assert_eq!(delta_wrap(190, 10, Some(100)), None);
    }

    #[test]
    fn rates_handle_gaps() {
        let mut r = Rates::default();
        let t0 = Instant::now();
        let iv = Duration::from_secs(1);
        assert_eq!(r.rate("a", 100, t0, iv), None);
        let v = r.rate("a", 300, t0 + Duration::from_secs(2), iv).unwrap();
        assert!((v - 100.0).abs() < 1e-9);
        // suspend: 1 hour gap -> no rate
        assert_eq!(
            r.rate("a", 999_999, t0 + Duration::from_secs(3602), iv),
            None
        );
        // reset -> no rate
        assert_eq!(r.rate("a", 5, t0 + Duration::from_secs(3603), iv), None);
        let v = r.rate("a", 15, t0 + Duration::from_secs(3604), iv).unwrap();
        assert!((v - 10.0).abs() < 1e-9);
    }

    #[test]
    fn tick_clock() {
        let mut c = TickClock::default();
        let t0 = Instant::now();
        let iv = Duration::from_secs(1);
        assert_eq!(c.tick(t0, iv), None);
        assert!(c.tick(t0 + Duration::from_millis(1000), iv).is_some());
        assert_eq!(c.tick(t0 + Duration::from_secs(100), iv), None);
    }
}
