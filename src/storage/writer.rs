//! Dedicated writer thread. It owns the only read/write connection, receives
//! work through a bounded queue (the oldest batch is dropped when full) and
//! runs maintenance. Database errors never stop the thread; they are logged
//! (rate limited), exposed in the status and followed by a backoff.

use super::rollup;
use crate::model::Desc;
use crate::settings::Settings;
use crate::util::time::{now_ms, now_s};
use anyhow::Result;
use rusqlite::{Connection, params};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub enum Msg {
    Persist {
        ts: i64,
        rows: Vec<(Desc, f64)>,
    },
    Settings(String),
    /// Upsert one row of the `meta` table.
    Meta {
        key: String,
        value: String,
    },
    Flush(std::sync::mpsc::Sender<()>),
    Shutdown,
}

pub struct Queue {
    inner: Mutex<VecDeque<Msg>>,
    cv: Condvar,
    cap: usize,
    pub dropped: AtomicU64,
}

impl Queue {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            cap,
            dropped: AtomicU64::new(0),
        }
    }

    pub fn push(&self, msg: Msg) {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= self.cap
            && let Some(pos) = q.iter().position(|m| matches!(m, Msg::Persist { .. }))
        {
            q.remove(pos);
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        q.push_back(msg);
        self.cv.notify_one();
    }

    pub fn pop_timeout(&self, d: Duration) -> Option<Msg> {
        let q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (mut q, _) = self
            .cv
            .wait_timeout_while(q, d, |q| q.is_empty())
            .unwrap_or_else(|e| e.into_inner());
        q.pop_front()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|q| q.len()).unwrap_or(0)
    }
}

#[derive(Default)]
pub struct WriterState {
    pub last_ok_ms: AtomicI64,
    pub heartbeat_ms: AtomicI64,
    pub rows: AtomicU64,
    pub failed_batches: AtomicU64,
    pub db_bytes: AtomicU64,
    pub last_error: Mutex<Option<String>>,
    pub last_maintenance_ms: AtomicI64,
}

impl WriterState {
    fn set_error(&self, e: Option<String>) {
        *self.last_error.lock().unwrap_or_else(|x| x.into_inner()) = e;
    }

    pub fn error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|x| x.into_inner())
            .clone()
    }
}

struct Writer {
    conn: Connection,
    ids: HashMap<String, (i64, Desc)>,
    seen: HashSet<i64>,
    state: Arc<WriterState>,
    settings: watch::Receiver<Arc<Settings>>,
    fails: u32,
    next_minute: Instant,
    next_hour: Instant,
}

impl Writer {
    fn series_id(&mut self, desc: &Desc, ts: i64) -> Result<i64> {
        if let Some((id, d)) = self.ids.get(&desc.key)
            && (Arc::ptr_eq(d, desc) || **d == **desc)
        {
            return Ok(*id);
        }
        let id: i64 = self.conn.prepare_cached(
            "INSERT INTO series(key, collector, metric, grp, group_label, label, unit, first_seen, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
             ON CONFLICT(key) DO UPDATE SET collector=excluded.collector, metric=excluded.metric,
               grp=excluded.grp, group_label=excluded.group_label, label=excluded.label,
               unit=excluded.unit, last_seen=excluded.last_seen
             RETURNING id",
        )?
        .query_row(
            params![desc.key, desc.collector, desc.metric, desc.group, desc.group_label, desc.label, desc.unit, ts],
            |r| r.get(0),
        )?;
        self.ids.insert(desc.key.clone(), (id, desc.clone()));
        Ok(id)
    }

    fn persist(&mut self, ts: i64, rows: &[(Desc, f64)]) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let r = (|| -> Result<()> {
            for (desc, v) in rows {
                let id = self.series_id(desc, ts)?;
                self.seen.insert(id);
                self.conn
                    .prepare_cached(
                        "INSERT INTO samples(series_id, ts, value) VALUES (?1, ?2, ?3)
                         ON CONFLICT(series_id, ts) DO UPDATE SET value=excluded.value",
                    )?
                    .execute(params![id, ts, v])?;
            }
            Ok(())
        })();
        match r {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                self.state
                    .rows
                    .fetch_add(rows.len() as u64, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                // Cached ids may refer to rolled back inserts.
                self.ids.clear();
                Err(e)
            }
        }
    }

    fn save_settings(&mut self, json: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings(key, value) VALUES ('settings', ?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [json],
        )?;
        Ok(())
    }

    fn maintenance(&mut self, hourly: bool) -> Result<()> {
        let now = now_s();
        rollup::rollup_minutes(&mut self.conn, now)?;
        if hourly {
            let settings = self.settings.borrow().clone();
            rollup::rollup_hours(&mut self.conn, now)?;
            if !self.seen.is_empty() {
                let tx = self.conn.transaction()?;
                {
                    let mut st = tx.prepare_cached("UPDATE series SET last_seen=?1 WHERE id=?2")?;
                    for id in self.seen.drain() {
                        st.execute(params![now, id])?;
                    }
                }
                tx.commit()?;
            }
            rollup::prune(&mut self.conn, &settings.history, now)?;
            let max = settings.history.max_db_mb * 1024 * 1024;
            if rollup::enforce_size(&mut self.conn, max, now)? {
                crate::warn_limited!(
                    "storage:size_cap",
                    "database exceeded the {} MB size cap; oldest history was removed early",
                    settings.history.max_db_mb
                );
            }
            rollup::vacuum_and_checkpoint(&self.conn)?;
            self.state
                .last_maintenance_ms
                .store(now_ms(), Ordering::Relaxed);
        }
        self.state.db_bytes.store(
            rollup::used_bytes(&self.conn).unwrap_or(0),
            Ordering::Relaxed,
        );
        Ok(())
    }

    fn on_result(&mut self, what: &str, r: Result<()>) {
        match r {
            Ok(()) => {
                if self.fails > 0 {
                    tracing::info!("database writes recovered");
                    crate::util::ratelimit::clear("storage:write");
                }
                self.fails = 0;
                self.state.last_ok_ms.store(now_ms(), Ordering::Relaxed);
                self.state.set_error(None);
            }
            Err(e) => {
                self.fails += 1;
                self.state.failed_batches.fetch_add(1, Ordering::Relaxed);
                let msg = format!("{what}: {e:#}");
                crate::warn_limited!(
                    "storage:write",
                    "database error ({msg}); live view continues"
                );
                self.state.set_error(Some(msg));
                // Back off on persistent errors (disk full, I/O errors).
                let secs = 1u64 << self.fails.min(6);
                std::thread::sleep(Duration::from_secs(secs.min(60)));
            }
        }
    }

    fn checkpoint(&self) {
        let _ = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                r.get::<_, i64>(0)
            });
    }
}

pub fn spawn(
    conn: Connection,
    queue: Arc<Queue>,
    state: Arc<WriterState>,
    settings: watch::Receiver<Arc<Settings>>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("hwstats-db".into())
        .spawn(move || {
            state
                .db_bytes
                .store(rollup::used_bytes(&conn).unwrap_or(0), Ordering::Relaxed);
            let mut w = Writer {
                conn,
                ids: HashMap::new(),
                seen: HashSet::new(),
                state,
                settings,
                fails: 0,
                next_minute: Instant::now() + Duration::from_secs(60),
                next_hour: Instant::now() + Duration::from_secs(300),
            };
            loop {
                w.state.heartbeat_ms.store(now_ms(), Ordering::Relaxed);
                match queue.pop_timeout(Duration::from_secs(1)) {
                    Some(Msg::Persist { ts, rows }) => {
                        let r = w.persist(ts, &rows);
                        w.on_result("insert", r);
                    }
                    Some(Msg::Settings(json)) => {
                        let r = w.save_settings(&json);
                        w.on_result("save settings", r);
                    }
                    Some(Msg::Meta { key, value }) => {
                        let r = super::schema::meta_set(&w.conn, &key, &value);
                        w.on_result("save meta", r);
                    }
                    Some(Msg::Flush(tx)) => {
                        w.checkpoint();
                        let _ = tx.send(());
                    }
                    Some(Msg::Shutdown) => {
                        while let Some(m) = queue.pop_timeout(Duration::ZERO) {
                            match m {
                                Msg::Persist { ts, rows } => {
                                    let _ = w.persist(ts, &rows);
                                }
                                Msg::Settings(json) => {
                                    let _ = w.save_settings(&json);
                                }
                                Msg::Meta { key, value } => {
                                    let _ = super::schema::meta_set(&w.conn, &key, &value);
                                }
                                Msg::Flush(tx) => {
                                    let _ = tx.send(());
                                }
                                Msg::Shutdown => {}
                            }
                        }
                        w.checkpoint();
                        tracing::info!("database writer stopped cleanly");
                        break;
                    }
                    None => {}
                }
                let now = Instant::now();
                if now >= w.next_minute {
                    let hourly = now >= w.next_hour;
                    w.next_minute = now + Duration::from_secs(60);
                    if hourly {
                        w.next_hour = now + Duration::from_secs(3600);
                    }
                    let r = w.maintenance(hourly);
                    w.on_result("maintenance", r);
                }
            }
        })
}

/// Load the persisted settings document, if any.
pub fn load_settings(conn: &Connection) -> Option<String> {
    conn.query_row("SELECT value FROM settings WHERE key='settings'", [], |r| {
        r.get(0)
    })
    .ok()
}
