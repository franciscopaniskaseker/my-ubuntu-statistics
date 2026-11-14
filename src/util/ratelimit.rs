//! Rate-limited logging so a persistently failing source cannot flood the
//! journal. The first occurrence of a key is logged, then at most once every
//! [`PERIOD`], together with the number of suppressed repeats.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

pub const PERIOD: Duration = Duration::from_secs(600);

static STATE: LazyLock<Mutex<HashMap<String, (Instant, u64)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Returns `Some(suppressed_count)` when the message should be logged now.
pub fn should_log(key: &str) -> Option<u64> {
    let now = Instant::now();
    let mut map = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if map.len() > 10_000 {
        map.clear();
    }
    match map.get_mut(key) {
        None => {
            map.insert(key.to_string(), (now, 0));
            Some(0)
        }
        Some((last, suppressed)) => {
            if now.duration_since(*last) >= PERIOD {
                let n = *suppressed;
                *last = now;
                *suppressed = 0;
                Some(n)
            } else {
                *suppressed += 1;
                None
            }
        }
    }
}

/// Forget a key so the next failure is logged immediately (used on recovery).
pub fn clear(key: &str) {
    let mut map = STATE.lock().unwrap_or_else(|e| e.into_inner());
    map.remove(key);
}

#[macro_export]
macro_rules! warn_limited {
    ($key:expr, $($arg:tt)+) => {{
        if let Some(n) = $crate::util::ratelimit::should_log(AsRef::<str>::as_ref(&$key)) {
            if n > 0 {
                tracing::warn!("{} (repeated {} times since last report)", format_args!($($arg)+), n);
            } else {
                tracing::warn!($($arg)+);
            }
        }
    }};
}
