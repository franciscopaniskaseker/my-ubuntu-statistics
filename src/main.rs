//! hwstats: local hardware statistics dashboard with history.

mod backup;
mod collectors;
mod config;
mod derived;
mod health;
mod hostinfo;
mod http;
mod hub;
mod model;
mod procinfo;
mod settings;
mod storage;
mod util;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use config::{Config, Overrides};
use settings::Settings;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize};
use std::time::Duration;
use tokio::sync::watch;
use util::sysfs::Roots;
use util::time::now_ms;

#[derive(Parser)]
#[command(
    name = "hwstats",
    version,
    about = "Local hardware statistics dashboard with history"
)]
struct Cli {
    /// Path to the configuration file.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Listen address (overrides the config file), e.g. 127.0.0.1:9393.
    #[arg(long, global = true)]
    listen: Option<String>,
    /// Database file (overrides the config file).
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    /// Backup directory (overrides the config file).
    #[arg(long, global = true)]
    backup_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the service (default).
    Serve,
    /// Show which statistics are available and hints for missing ones.
    Doctor,
    /// Create a compressed backup of the database now.
    Backup,
    /// Restore the database from a backup file (service must be stopped).
    Restore {
        /// Backup file (*.db.gz).
        file: PathBuf,
    },
}

fn init_logging(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(format!("hwstats={level},warn")))
        .unwrap_or_else(|_| EnvFilter::new("hwstats=info,warn"));
    let journald = std::env::var_os("JOURNAL_STREAM").is_some();
    let b = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr);
    if journald {
        b.without_time().with_ansi(false).init();
    } else {
        b.init();
    }
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown".into());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic".into());
        if util::ratelimit::should_log(&format!("panic:{loc}")).is_some() {
            let bt = std::backtrace::Backtrace::force_capture();
            tracing::error!("panic at {loc}: {msg}\n{bt}");
        }
    }));
}

fn main() {
    let cli = Cli::parse();
    let cfg = Config::load(
        cli.config.as_deref(),
        Overrides {
            listen: cli.listen.clone(),
            database: cli.db.clone(),
            backup_dir: cli.backup_dir.clone(),
        },
    );
    init_logging(&cfg.log_level);
    install_panic_hook();
    for w in &cfg.warnings {
        tracing::warn!("{w}");
    }
    let res = match cli.cmd.unwrap_or(Cmd::Serve) {
        Cmd::Serve => serve(cfg),
        Cmd::Doctor => doctor(cfg),
        Cmd::Backup => backup_cmd(cfg),
        Cmd::Restore { file } => restore_cmd(cfg, file),
    };
    if let Err(e) = res {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn serve(cfg: Config) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(32)
        .thread_name("hwstats-rt")
        .enable_all()
        .build()
        .context("starting async runtime")?;
    rt.block_on(serve_async(cfg))
}

async fn serve_async(cfg: Config) -> Result<()> {
    tracing::info!("hwstats {} starting", env!("CARGO_PKG_VERSION"));
    let mut notes: Vec<String> = Vec::new();

    // State directory and single-instance lock.
    let mut lock = None;
    let mut storage_error: Option<String> = None;
    match std::fs::create_dir_all(&cfg.state_dir) {
        Ok(()) => match util::lock::acquire(&cfg.state_dir.join("hwstats.lock")) {
            Ok(l) => lock = Some(l),
            Err(util::lock::LockError::Held) => {
                bail!(
                    "another hwstats instance is already using {} (stop it first, e.g. sudo systemctl stop hwstats)",
                    cfg.state_dir.display()
                );
            }
            Err(e) => storage_error = Some(e.to_string()),
        },
        Err(e) => storage_error = Some(format!("cannot create {}: {e}", cfg.state_dir.display())),
    }

    // Database (optional).
    let opened = if storage_error.is_none() {
        match storage::open(&cfg.database, &cfg.backup_dir) {
            Ok(o) => Some(o),
            Err(e) => {
                storage_error = Some(format!("{e:#}"));
                None
            }
        }
    } else {
        None
    };
    if let Some(e) = &storage_error {
        tracing::error!("history disabled, running in memory-only mode: {e}");
    }

    let mut settings = Settings::default();
    if let Some(o) = &opened {
        notes.extend(o.notes.iter().cloned());
        if let Some(j) = &o.settings_json {
            let (s, err) = Settings::from_json_lenient(j);
            if let Some(e) = err {
                tracing::warn!("stored settings are invalid ({e}); using defaults");
                notes.push("stored settings were invalid and have been reset".into());
            }
            settings = s;
        }
    }
    for n in &notes {
        tracing::warn!("{n}");
    }
    let (settings_tx, settings_rx) = watch::channel(Arc::new(settings));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let storage = match opened {
        Some(o) => match storage::Storage::start(cfg.database.clone(), o, settings_rx.clone()) {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                storage_error = Some(format!("cannot start database writer: {e:#}"));
                None
            }
        },
        None => None,
    };

    let state_dir = if lock.is_some() {
        cfg.state_dir.clone()
    } else {
        std::env::temp_dir()
    };
    let roots = Roots::from_env(state_dir);
    let hub = Arc::new(hub::Hub::new());

    // Collectors.
    for spec in collectors::all(&roots) {
        tokio::spawn(collectors::run(
            spec,
            settings_rx.clone(),
            hub.clone(),
            shutdown_rx.clone(),
        ));
    }

    // Persistence of averaged buckets.
    if let Some(st) = storage.clone() {
        let (hub, mut srx, mut sd) = (hub.clone(), settings_rx.clone(), shutdown_rx.clone());
        tokio::spawn(async move {
            loop {
                let res = srx.borrow_and_update().history.resolution_s.max(1) as i64;
                let now = now_ms();
                let next = ((now / 1000) / res + 1) * res * 1000;
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis((next - now).max(10) as u64)) => {}
                    _ = srx.changed() => continue,
                    _ = sd.changed() => break,
                }
                let rows = hub.take_pending();
                st.persist(next / 1000 - res, rows);
            }
        });
    }

    // Derived series (power summary).
    tokio::spawn(derived::run(
        hub.clone(),
        settings_rx.clone(),
        shutdown_rx.clone(),
    ));

    // Live catalog housekeeping.
    {
        let (hub, mut sd) = (hub.clone(), shutdown_rx.clone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(60)) => hub.prune_stale(now_ms()),
                    _ = sd.changed() => break,
                }
            }
        });
    }

    // Backups.
    let backups = Arc::new(backup::Manager::new(
        cfg.database.clone(),
        cfg.backup_dir.clone(),
    ));
    if storage.is_some() {
        tokio::spawn(
            backups
                .clone()
                .run_scheduler(settings_rx.clone(), shutdown_rx.clone()),
        );
    }

    // Watchdog.
    let hb = Arc::new(AtomicI64::new(now_ms()));
    tokio::spawn(health::heartbeat(hb.clone(), shutdown_rx.clone()));
    health::spawn_watchdog(hb);

    // HTTP.
    let app_state = Arc::new(http::AppState {
        hub: hub.clone(),
        storage: storage.clone(),
        storage_error: storage_error.clone(),
        settings_tx,
        backups,
        config: cfg.clone(),
        host: hostinfo::collect(&roots),
        roots: roots.clone(),
        proc_slots: Arc::new(tokio::sync::Semaphore::new(http::PROC_SLOTS)),
        notes,
        sse_clients: AtomicUsize::new(0),
    });
    let policy = http::guard::HostPolicy::new(cfg.listen, &cfg.allowed_hosts);
    if policy.allows_any() && !cfg.listen.ip().is_loopback() {
        tracing::warn!(
            "listening on {} without allowed_hosts: any Host header is accepted; this page is meant for local use only",
            cfg.listen
        );
    }
    let app = http::router(app_state);
    let listen = cfg.listen;
    let http_task = {
        let mut sd = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                let listener = loop {
                    match tokio::net::TcpListener::bind(listen).await {
                        Ok(l) => break l,
                        Err(e) => {
                            warn_limited!(
                                "http:bind",
                                "cannot listen on {listen}: {e}; retrying every 10 s (collection continues)"
                            );
                            health::status(&format!("waiting for {listen}: {e}"));
                            tokio::select! {
                                _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                                _ = sd.changed() => return,
                            }
                        }
                    }
                };
                tracing::info!("dashboard available at http://{listen}/");
                health::status(&format!("serving http://{listen}/"));
                let mut sd2 = sd.clone();
                let r = axum::serve(listener, app.clone())
                    .with_graceful_shutdown(async move {
                        let _ = sd2.changed().await;
                    })
                    .await;
                if *sd.borrow() {
                    return;
                }
                if let Err(e) = r {
                    tracing::error!("http server stopped: {e}; restarting");
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
    };

    health::ready();
    wait_for_signal().await;
    tracing::info!("shutting down");
    health::stopping();
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), http_task).await;
    if let Some(st) = storage {
        st.persist(now_ms() / 1000, hub.take_pending());
        let _ = tokio::task::spawn_blocking(move || st.shutdown(Duration::from_secs(10))).await;
    }
    drop(lock);
    Ok(())
}

async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("cannot install SIGTERM handler: {e}");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

/// Minimal HTTP GET against the running service (no extra dependencies).
fn local_get(addr: std::net::SocketAddr, path: &str) -> Result<String> {
    let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1))?;
    s.set_read_timeout(Some(Duration::from_secs(3)))?;
    write!(
        s,
        "GET {path} HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )?;
    let mut buf = String::new();
    s.read_to_string(&mut buf)?;
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") {
        bail!("unexpected response: {}", head.lines().next().unwrap_or(""));
    }
    Ok(body.to_string())
}

fn print_row(name: &str, state: &str, source: &str, series: u64, reason: &str, hint: &str) {
    println!("  {name:<12} {state:<12} {series:>4}  {source}");
    if !reason.is_empty() {
        println!("  {:<12} reason: {reason}", "");
    }
    if !hint.is_empty() {
        println!("  {:<12} hint:   {hint}", "");
    }
}

fn doctor(cfg: Config) -> Result<()> {
    println!("hwstats {} doctor", env!("CARGO_PKG_VERSION"));
    println!("  config listen : {}", cfg.listen);
    println!("  database      : {}", cfg.database.display());
    println!("  backups       : {}", cfg.backup_dir.display());
    println!();
    let addr = if cfg.listen.ip().is_unspecified() {
        std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), cfg.listen.port())
    } else {
        cfg.listen
    };
    match local_get(addr, "/api/status")
        .and_then(|b| Ok(serde_json::from_str::<serde_json::Value>(&b)?))
    {
        Ok(v) => {
            println!("Service is running at http://{addr}/ (status from the service):");
            println!("  {:<12} {:<12} {:>4}  source", "collector", "state", "n");
            for c in v["collectors"].as_array().cloned().unwrap_or_default() {
                print_row(
                    c["name"].as_str().unwrap_or("?"),
                    c["state"].as_str().unwrap_or("?"),
                    c["source"].as_str().unwrap_or(""),
                    c["series"].as_u64().unwrap_or(0),
                    c["reason"].as_str().unwrap_or(""),
                    c["hint"].as_str().unwrap_or(""),
                );
            }
            println!();
            if v["memory_only"].as_bool() == Some(true) {
                println!(
                    "  history: DISABLED ({})",
                    v["storage_error"].as_str().unwrap_or("unknown")
                );
            } else {
                let s = &v["storage"];
                let last = s["last_write_ms"].as_i64().unwrap_or(0);
                let last = if last > 0 {
                    format!("{} s ago", (now_ms() - last) / 1000)
                } else {
                    "not yet".to_string()
                };
                println!(
                    "  history: {:.1} MB in database, last write {last}{}",
                    s["db_bytes"].as_u64().unwrap_or(0) as f64 / 1_048_576.0,
                    s["error"]
                        .as_str()
                        .map(|e| format!(", error: {e}"))
                        .unwrap_or_default()
                );
            }
            let b = &v["backups"];
            println!(
                "  backups: {} file(s), latest {}",
                b["count"].as_u64().unwrap_or(0),
                b["latest"]["file"].as_str().unwrap_or("none")
            );
        }
        Err(e) => {
            println!("Service not reachable at {addr} ({e}); probing locally as the current user:");
            println!("  {:<12} {:<12} {:>4}  source", "collector", "state", "n");
            let roots = Roots::from_env(std::env::temp_dir());
            for (name, h, n) in collectors::probe_all(&roots) {
                let state = serde_json::to_value(h.state)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default();
                print_row(
                    name,
                    &state,
                    &h.source,
                    n as u64,
                    h.reason.as_deref().unwrap_or(""),
                    h.hint.as_deref().unwrap_or(""),
                );
            }
            println!();
            println!(
                "  note: results can differ from the service, which runs with CAP_DAC_READ_SEARCH."
            );
        }
    }
    Ok(())
}

fn backup_cmd(cfg: Config) -> Result<()> {
    let settings = rusqlite::Connection::open_with_flags(
        &cfg.database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()
    .and_then(|c| storage::writer::load_settings(&c))
    .map(|j| Settings::from_json_lenient(&j).0)
    .unwrap_or_default();
    let info = backup::create(&cfg.database, &cfg.backup_dir)?;
    println!(
        "backup written: {} ({} KB)",
        info.path.display(),
        info.size / 1024
    );
    let b = settings.backups;
    for d in backup::rotate(
        &cfg.backup_dir,
        b.keep_daily,
        b.keep_weekly,
        b.max_total_mb * 1024 * 1024,
    ) {
        println!("rotated out: {}", d.display());
    }
    Ok(())
}

fn restore_cmd(cfg: Config, file: PathBuf) -> Result<()> {
    let _lock = match util::lock::acquire(&cfg.state_dir.join("hwstats.lock")) {
        Ok(l) => l,
        Err(util::lock::LockError::Held) => {
            bail!("the hwstats service is running; stop it first: sudo systemctl stop hwstats")
        }
        Err(e) => bail!("{e}"),
    };
    backup::restore_file(&file, &cfg.database)?;
    println!("database restored from {}", file.display());
    println!("start the service again: sudo systemctl start hwstats");
    Ok(())
}
