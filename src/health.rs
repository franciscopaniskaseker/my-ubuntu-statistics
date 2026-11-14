//! systemd integration: readiness, status text and the watchdog.
//!
//! The watchdog thread only pings systemd while the async runtime keeps
//! updating a heartbeat. If the runtime wedges, pings stop and systemd
//! restarts the service (`WatchdogSec`). Outside systemd all of this is a
//! no-op.

use crate::util::time::now_ms;
use sd_notify::NotifyState;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

const MAX_HEARTBEAT_AGE_MS: i64 = 20_000;

pub fn ready() {
    let _ = sd_notify::notify(&[NotifyState::Ready, NotifyState::Status("collecting")]);
}

pub fn stopping() {
    let _ = sd_notify::notify(&[NotifyState::Stopping]);
}

pub fn status(s: &str) {
    let _ = sd_notify::notify(&[NotifyState::Status(s)]);
}

/// Async task that proves the runtime is alive.
pub async fn heartbeat(hb: Arc<AtomicI64>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    loop {
        hb.store(now_ms(), Ordering::Relaxed);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            _ = shutdown.changed() => break,
        }
    }
}

/// Start the watchdog thread when systemd asked for one.
pub fn spawn_watchdog(hb: Arc<AtomicI64>) {
    let Some(period) = sd_notify::watchdog_enabled() else {
        return;
    };
    let every = (period / 3).max(Duration::from_secs(1));
    tracing::info!(
        "systemd watchdog enabled ({:?}); pinging every {:?}",
        period,
        every
    );
    let _ = std::thread::Builder::new()
        .name("hwstats-watchdog".into())
        .spawn(move || {
            loop {
                std::thread::sleep(every);
                let age = now_ms() - hb.load(Ordering::Relaxed);
                if age < MAX_HEARTBEAT_AGE_MS {
                    let _ = sd_notify::notify(&[NotifyState::Watchdog]);
                } else {
                    tracing::error!("runtime heartbeat is {age} ms old; withholding watchdog ping");
                }
            }
        });
}
