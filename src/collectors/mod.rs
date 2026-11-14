//! Collectors and their supervised scheduler.
//!
//! Each collector runs in its own task. A tick executes on the blocking pool,
//! wrapped in `catch_unwind` and a timeout:
//! * a panic drops the collector instance; a fresh one is built after an
//!   exponential backoff, other collectors are unaffected;
//! * a tick that does not finish in time marks the collector `stalled` and no
//!   new tick is started until the stuck one returns, so blocked threads
//!   cannot pile up;
//! * a collector reporting `unavailable` is re-probed every 30 s only;
//! * a hardware rescan (see `crate::inventory`) drops the instances of the
//!   device-discovering collectors listed in [`REDISCOVER`] and ticks them
//!   at once, cutting short re-probe waits and error backoff.

pub mod battery;
pub mod cpu;
pub mod disk;
pub mod filesystem;
pub mod gpu_drm;
pub mod gpu_nvidia;
pub mod hwmon;
pub mod memory;
pub mod network;
pub mod power;
pub mod pressure;
pub mod processes;
pub mod system;
pub mod thermal;

use crate::hub::Hub;
use crate::model::{Health, Out, State};
use crate::settings::Settings;
use crate::util::sysfs::Roots;
use crate::util::time::now_ms;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Context passed to every tick.
pub struct Ctx<'a> {
    pub now: Instant,
    pub wall_ms: i64,
    pub interval: Duration,
    pub settings: &'a Settings,
}

pub trait Collector: Send + 'static {
    /// Collect one sample set. Must not panic on missing/odd data; return
    /// `Err` only when the whole source failed for this tick.
    fn collect(&mut self, ctx: &Ctx, out: &mut Out) -> anyhow::Result<()>;
}

type Factory = Arc<dyn Fn() -> Box<dyn Collector> + Send + Sync>;

/// Collectors that discover devices; a hardware rescan rebuilds them.
pub const REDISCOVER: &[&str] = &[
    "disk",
    "network",
    "sensors",
    "thermal",
    "power",
    "battery",
    "gpu_nvidia",
    "gpu_drm",
];

#[derive(Clone)]
pub struct Spec {
    pub name: &'static str,
    pub make: Factory,
}

fn spec<C: Collector>(name: &'static str, f: impl Fn() -> C + Send + Sync + 'static) -> Spec {
    Spec {
        name,
        make: Arc::new(move || Box::new(f()) as Box<dyn Collector>),
    }
}

/// All collectors, in display order.
pub fn all(roots: &Roots) -> Vec<Spec> {
    macro_rules! s {
        ($name:expr, $ty:path) => {{
            let r = roots.clone();
            spec($name, move || <$ty>::new(r.clone()))
        }};
    }
    vec![
        s!("cpu", cpu::Cpu),
        s!("system", system::System),
        s!("pressure", pressure::Pressure),
        s!("memory", memory::Memory),
        s!("disk", disk::Disk),
        s!("filesystem", filesystem::Filesystem),
        s!("network", network::Network),
        s!("sensors", hwmon::Hwmon),
        s!("thermal", thermal::Thermal),
        s!("power", power::Power),
        s!("battery", battery::Battery),
        s!("gpu_nvidia", gpu_nvidia::GpuNvidia),
        s!("gpu_drm", gpu_drm::GpuDrm),
        s!("processes", processes::Processes),
    ]
}

enum TickOutcome {
    Ok,
    Err(String),
    Panic(String),
}

struct TickResult {
    collector: Option<Box<dyn Collector>>,
    out: Out,
    outcome: TickOutcome,
    duration: Duration,
    wall_ms: i64,
}

fn panic_msg(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".into()
    }
}

fn run_tick(
    existing: Option<Box<dyn Collector>>,
    make: Factory,
    settings: Arc<Settings>,
    interval: Duration,
) -> TickResult {
    let t0 = Instant::now();
    let wall_ms = now_ms();
    let mut out = Out::new();
    let res = catch_unwind(AssertUnwindSafe(|| {
        let mut c = match existing {
            Some(c) => c,
            None => make(),
        };
        let ctx = Ctx {
            now: t0,
            wall_ms,
            interval,
            settings: &settings,
        };
        let r = c.collect(&ctx, &mut out);
        (c, r)
    }));
    let (collector, outcome) = match res {
        Ok((c, Ok(()))) => (Some(c), TickOutcome::Ok),
        Ok((c, Err(e))) => (Some(c), TickOutcome::Err(format!("{e:#}"))),
        Err(p) => (None, TickOutcome::Panic(panic_msg(p.as_ref()))),
    };
    TickResult {
        collector,
        out,
        outcome,
        duration: t0.elapsed(),
        wall_ms,
    }
}

const UNAVAILABLE_RECHECK: Duration = Duration::from_secs(30);

/// Supervised loop for one collector.
pub async fn run(
    spec: Spec,
    mut settings_rx: watch::Receiver<Arc<Settings>>,
    hub: Arc<Hub>,
    mut rescan: watch::Receiver<u64>,
    mut shutdown: watch::Receiver<bool>,
) {
    let name = spec.name;
    let rediscover = REDISCOVER.contains(&name);
    let mut generation = *rescan.borrow_and_update();
    let mut collector: Option<Box<dyn Collector>> = None;
    // A tick still running, with the rescan generation it started in.
    let mut inflight: Option<(tokio::task::JoinHandle<TickResult>, u64)> = None;
    let mut fails: u32 = 0;
    let mut panics: u32 = 0;
    let mut next = tokio::time::Instant::now();

    loop {
        if *shutdown.borrow() {
            break;
        }
        let current = *rescan.borrow_and_update();
        if current != generation {
            generation = current;
            if rediscover {
                // Fresh instance: devices, names and caches are found again.
                collector = None;
                fails = 0;
                panics = 0;
            }
        }
        let settings = settings_rx.borrow_and_update().clone();
        let cfg = settings.collector(name);
        if !cfg.enabled {
            hub.update_status(name, |s| {
                s.state = State::Disabled;
                s.reason = None;
                s.series = 0;
            });
            // Drop state so a re-enable starts clean.
            collector = None;
            tokio::select! {
                _ = settings_rx.changed() => { next = tokio::time::Instant::now(); continue; }
                _ = shutdown.changed() => break,
            }
        }
        let interval = Duration::from_millis(cfg.interval_ms);
        hub.update_status(name, |s| s.interval_ms = cfg.interval_ms);

        tokio::select! {
            _ = tokio::time::sleep_until(next) => {}
            r = settings_rx.changed() => {
                if r.is_err() { break; }
                // Re-evaluate the schedule with the new interval.
                let now = tokio::time::Instant::now();
                let s = settings_rx.borrow().collector(name);
                let candidate = now + Duration::from_millis(s.interval_ms).min(next.saturating_duration_since(now));
                next = candidate;
                continue;
            }
            r = rescan.changed() => {
                if r.is_err() { break; }
                if rediscover {
                    next = tokio::time::Instant::now();
                }
                continue;
            }
            _ = shutdown.changed() => break,
        }

        let tick_timeout = (interval * 2).clamp(Duration::from_secs(2), Duration::from_secs(10));
        let (mut handle, tick_generation) = match inflight.take() {
            Some(h) => h,
            None => {
                let make = spec.make.clone();
                let c = collector.take();
                let s = settings.clone();
                (
                    tokio::task::spawn_blocking(move || run_tick(c, make, s, interval)),
                    generation,
                )
            }
        };

        let result = match tokio::time::timeout(tick_timeout, &mut handle).await {
            Ok(Ok(r)) => r,
            Ok(Err(join_err)) => {
                // Should not happen (panics are caught inside), but stay alive.
                panics += 1;
                let msg = format!("collector task failed: {join_err}");
                crate::warn_limited!(format!("collector:{name}:join"), "{name}: {msg}");
                hub.update_status(name, |s| {
                    s.state = State::Error;
                    s.reason = Some(msg.clone());
                    s.restarts += 1;
                });
                next = tokio::time::Instant::now() + panic_backoff(panics);
                continue;
            }
            Err(_) => {
                inflight = Some((handle, tick_generation));
                crate::warn_limited!(
                    format!("collector:{name}:stalled"),
                    "{name}: tick did not finish within {:?}; waiting for it before starting a new one",
                    tick_timeout
                );
                hub.update_status(name, |s| {
                    s.state = State::Stalled;
                    s.reason = Some(format!("tick exceeded {} ms", tick_timeout.as_millis()));
                });
                next = tokio::time::Instant::now() + interval;
                continue;
            }
        };

        let TickResult {
            collector: c,
            out,
            outcome,
            duration,
            wall_ms,
        } = result;
        // An instance from before a rescan is dropped when its tick returns.
        collector = if rediscover && tick_generation != *rescan.borrow() {
            None
        } else {
            c
        };
        let stale = duration > tick_timeout;

        match outcome {
            TickOutcome::Ok => {
                if fails > 0 || panics > 0 {
                    tracing::info!("{name}: recovered");
                    crate::util::ratelimit::clear(&format!("collector:{name}:err"));
                }
                fails = 0;
                panics = 0;
                let health: Health = out.health.clone();
                let unavailable = health.state == State::Unavailable && out.points.is_empty();
                let npoints = out.points.len();
                if !stale {
                    hub.publish(name, wall_ms, out);
                }
                hub.update_status(name, |s| {
                    s.state = health.state;
                    s.source = health.source.clone();
                    s.reason = health.reason.clone();
                    s.hint = health.hint.clone();
                    s.series = npoints;
                    s.last_duration_ms = duration.as_secs_f64() * 1000.0;
                    s.ticks += 1;
                    if !unavailable {
                        s.last_ok_ms = wall_ms;
                    }
                });
                let delay = if unavailable {
                    UNAVAILABLE_RECHECK.max(interval)
                } else {
                    interval
                };
                next = schedule_next(next, delay);
            }
            TickOutcome::Err(msg) => {
                fails += 1;
                crate::warn_limited!(format!("collector:{name}:err"), "{name}: {msg}");
                hub.update_status(name, |s| {
                    s.state = State::Error;
                    s.reason = Some(msg.clone());
                    s.errors += 1;
                    s.series = 0;
                });
                let backoff =
                    (interval * 2u32.saturating_pow(fails.min(6))).min(Duration::from_secs(60));
                next = tokio::time::Instant::now() + backoff;
            }
            TickOutcome::Panic(msg) => {
                panics += 1;
                crate::warn_limited!(
                    format!("collector:{name}:panic"),
                    "{name}: collector panicked ({msg}); restarting it after backoff"
                );
                hub.update_status(name, |s| {
                    s.state = State::Error;
                    s.reason = Some(format!("panic: {msg}"));
                    s.errors += 1;
                    s.restarts += 1;
                    s.series = 0;
                });
                next = tokio::time::Instant::now() + panic_backoff(panics);
            }
        }
    }
    tracing::debug!("{name}: collector loop stopped");
}

fn panic_backoff(n: u32) -> Duration {
    let secs = 5u64.saturating_mul(1u64 << n.saturating_sub(1).min(6));
    Duration::from_secs(secs.min(300))
}

fn schedule_next(prev: tokio::time::Instant, interval: Duration) -> tokio::time::Instant {
    let now = tokio::time::Instant::now();
    let n = prev + interval;
    if n <= now { now + interval } else { n }
}

/// Run every collector twice (to get rates) and report their health.
/// Used by `hwstats doctor` when the service is not reachable.
pub fn probe_all(roots: &Roots) -> Vec<(&'static str, Health, usize)> {
    let settings = Arc::new(Settings::default());
    let specs = all(roots);
    let mut results = Vec::new();
    let mut states: Vec<Option<Box<dyn Collector>>> = specs.iter().map(|_| None).collect();
    for round in 0..2 {
        for (i, sp) in specs.iter().enumerate() {
            let interval = Duration::from_millis(settings.collector(sp.name).interval_ms);
            let r = run_tick(
                states[i].take(),
                sp.make.clone(),
                settings.clone(),
                interval,
            );
            states[i] = r.collector;
            if round == 1 {
                let h = match r.outcome {
                    TickOutcome::Ok => r.out.health.clone(),
                    TickOutcome::Err(e) => Health {
                        state: State::Error,
                        source: String::new(),
                        reason: Some(e),
                        hint: None,
                    },
                    TickOutcome::Panic(e) => Health {
                        state: State::Error,
                        source: String::new(),
                        reason: Some(format!("panic: {e}")),
                        hint: None,
                    },
                };
                results.push((sp.name, h, r.out.points.len()));
            }
        }
        if round == 0 {
            std::thread::sleep(Duration::from_millis(1100));
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DescCache, SeriesDesc};

    struct Panicky;
    impl Collector for Panicky {
        fn collect(&mut self, _ctx: &Ctx, _out: &mut Out) -> anyhow::Result<()> {
            panic!("boom");
        }
    }

    struct Sleepy;
    impl Collector for Sleepy {
        fn collect(&mut self, _ctx: &Ctx, _out: &mut Out) -> anyhow::Result<()> {
            std::thread::sleep(Duration::from_secs(4));
            Ok(())
        }
    }

    struct Good(DescCache);
    impl Collector for Good {
        fn collect(&mut self, _ctx: &Ctx, out: &mut Out) -> anyhow::Result<()> {
            let d = self.0.get("test.value", || {
                SeriesDesc::new("cpu", "test.value", "v", "Value", "count")
            });
            out.push(&d, 1.0);
            out.health = Health::ok("test");
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn panics_and_stalls_are_isolated() {
        let hub = Arc::new(Hub::new());
        let mut s = Settings::default();
        for c in s.collectors.values_mut() {
            c.interval_ms = 250;
        }
        let (_stx, srx) = watch::channel(Arc::new(s));
        let (sdtx, sdrx) = watch::channel(false);
        let (_rtx, rrx) = watch::channel(0u64);
        let specs = vec![
            spec("cpu", || Good(DescCache::default())),
            spec("memory", || Panicky),
            spec("disk", || Sleepy),
        ];
        let mut handles = Vec::new();
        for sp in specs {
            handles.push(tokio::spawn(run(
                sp,
                srx.clone(),
                hub.clone(),
                rrx.clone(),
                sdrx.clone(),
            )));
        }
        tokio::time::sleep(Duration::from_millis(2600)).await;
        let st = hub.statuses();
        let get = |n: &str| st.iter().find(|s| s.name == n).unwrap().clone();
        assert_eq!(get("cpu").state, State::Ok);
        assert_eq!(get("memory").state, State::Error);
        assert!(get("memory").restarts >= 1);
        assert_eq!(get("disk").state, State::Stalled);
        assert!(hub.latest("test.value").is_some());
        sdtx.send(true).unwrap();
        for h in handles {
            let _ = tokio::time::timeout(Duration::from_secs(1), h).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rescan_rebuilds_device_collectors_only() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hub = Arc::new(Hub::new());
        let mut s = Settings::default();
        for c in s.collectors.values_mut() {
            c.interval_ms = 30_000;
        }
        let (_stx, srx) = watch::channel(Arc::new(s));
        let (sdtx, sdrx) = watch::channel(false);
        let (rtx, rrx) = watch::channel(0u64);
        let made_battery = Arc::new(AtomicUsize::new(0));
        let made_cpu = Arc::new(AtomicUsize::new(0));
        let (mb, mc) = (made_battery.clone(), made_cpu.clone());
        let specs = vec![
            spec("battery", move || {
                mb.fetch_add(1, Ordering::SeqCst);
                Good(DescCache::default())
            }),
            spec("cpu", move || {
                mc.fetch_add(1, Ordering::SeqCst);
                Good(DescCache::default())
            }),
        ];
        let mut handles = Vec::new();
        for sp in specs {
            handles.push(tokio::spawn(run(
                sp,
                srx.clone(),
                hub.clone(),
                rrx.clone(),
                sdrx.clone(),
            )));
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(made_battery.load(Ordering::SeqCst), 1);
        rtx.send(1).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        // The device collector was rebuilt and ticked at once (its 30 s
        // interval had not elapsed); the CPU collector kept its instance.
        assert_eq!(made_battery.load(Ordering::SeqCst), 2);
        assert_eq!(made_cpu.load(Ordering::SeqCst), 1);
        let st = hub.statuses();
        let ticks = |n: &str| st.iter().find(|s| s.name == n).unwrap().ticks;
        assert_eq!(ticks("battery"), 2);
        assert_eq!(ticks("cpu"), 1);
        sdtx.send(true).unwrap();
        for h in handles {
            let _ = tokio::time::timeout(Duration::from_secs(1), h).await;
        }
    }
}
