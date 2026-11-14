//! SQLite storage: open/recover/migrate, a writer thread and read queries.
//!
//! The database is never on the critical path: if it cannot be opened the
//! service runs in memory-only mode and the live view keeps working.

pub mod query;
pub mod recovery;
pub mod rollup;
pub mod schema;
pub mod writer;

use crate::model::Desc;
use crate::settings::Settings;
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use writer::{Msg, Queue, WriterState};

pub struct Opened {
    conn: Connection,
    pub settings_json: Option<String>,
    pub notes: Vec<String>,
}

/// Recover (if needed), open and migrate the database.
pub fn open(path: &Path, backup_dir: &Path) -> Result<Opened> {
    let mut notes = Vec::new();
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    match recovery::check_and_recover(path, backup_dir)? {
        recovery::Outcome::Restored(b) => notes.push(format!(
            "database was corrupt and has been restored from {}",
            b.display()
        )),
        recovery::Outcome::RecreatedAfterCorruption => {
            notes.push("database was corrupt and no backup was usable; history restarted".into())
        }
        _ => {}
    }
    let mut conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    schema::configure(&conn)?;
    let found = schema::version(&conn)?;
    if found > 0 && found < schema::VERSION {
        drop(conn);
        match crate::backup::create(path, backup_dir) {
            Ok(b) => notes.push(format!("backup {} taken before schema migration", b.file)),
            Err(e) => notes.push(format!("pre-migration backup failed: {e:#}")),
        }
        conn = Connection::open(path)?;
        schema::configure(&conn)?;
    }
    schema::migrate(&mut conn)?;
    let settings_json = writer::load_settings(&conn);
    Ok(Opened {
        conn,
        settings_json,
        notes,
    })
}

pub struct Storage {
    pub path: PathBuf,
    queue: Arc<Queue>,
    pub state: Arc<WriterState>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Storage {
    pub fn start(
        path: PathBuf,
        opened: Opened,
        settings: watch::Receiver<Arc<Settings>>,
    ) -> Result<Storage> {
        let queue = Arc::new(Queue::new(256));
        let state = Arc::new(WriterState::default());
        let handle = writer::spawn(opened.conn, queue.clone(), state.clone(), settings)?;
        Ok(Storage {
            path,
            queue,
            state,
            handle: Mutex::new(Some(handle)),
        })
    }

    pub fn persist(&self, ts: i64, rows: Vec<(Desc, f64)>) {
        if !rows.is_empty() {
            self.queue.push(Msg::Persist { ts, rows });
        }
    }

    pub fn save_settings(&self, s: &Settings) {
        if let Ok(j) = serde_json::to_string(s) {
            self.queue.push(Msg::Settings(j));
        }
    }

    /// Wait until queued work has been written (bounded wait).
    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        self.queue.push(Msg::Flush(tx));
        rx.recv_timeout(timeout).is_ok()
    }

    /// Flush and stop the writer thread.
    pub fn shutdown(&self, timeout: Duration) {
        self.queue.push(Msg::Shutdown);
        let h = self.handle.lock().ok().and_then(|mut g| g.take());
        if let Some(h) = h {
            let start = std::time::Instant::now();
            while !h.is_finished() && start.elapsed() < timeout {
                std::thread::sleep(Duration::from_millis(20));
            }
            if h.is_finished() {
                let _ = h.join();
            } else {
                tracing::warn!("database writer did not stop within {:?}", timeout);
            }
        }
    }

    pub fn status(&self) -> Value {
        let s = &self.state;
        json!({
            "path": self.path,
            "last_write_ms": s.last_ok_ms.load(Ordering::Relaxed),
            "heartbeat_ms": s.heartbeat_ms.load(Ordering::Relaxed),
            "rows_written": s.rows.load(Ordering::Relaxed),
            "failed_batches": s.failed_batches.load(Ordering::Relaxed),
            "dropped_batches": self.queue.dropped.load(Ordering::Relaxed),
            "queue": self.queue.len(),
            "db_bytes": s.db_bytes.load(Ordering::Relaxed),
            "last_maintenance_ms": s.last_maintenance_ms.load(Ordering::Relaxed),
            "error": s.error(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SeriesDesc;

    #[test]
    fn write_and_query_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("h.db");
        let opened = open(&db, &dir.path().join("b")).unwrap();
        assert!(opened.settings_json.is_none());
        let (_tx, rx) = watch::channel(Arc::new(Settings::default()));
        let st = Storage::start(db.clone(), opened, rx).unwrap();
        let d: Desc = Arc::new(SeriesDesc::new("cpu", "cpu.usage", "usage", "CPU", "%"));
        let now = crate::util::time::now_s() / 10 * 10;
        for i in 0..30 {
            st.persist(now - 300 + i * 10, vec![(d.clone(), i as f64)]);
        }
        st.save_settings(&Settings::default());
        assert!(st.flush(Duration::from_secs(5)));
        let h = Settings::default().history;
        let keys = vec!["cpu.usage".to_string()];
        let req = query::Request {
            keys: &keys,
            from: now - 600,
            to: now,
            points: 600,
            now,
            history: &h,
        };
        let (b, res) = query::history(Some(&db), &req, &|_, _| vec![], &|_| 1).unwrap();
        assert_eq!(b, 10);
        let s = &res["cpu.usage"];
        assert_eq!(s.t.len(), 30);
        assert_eq!(s.avg[29], Some(29.0));
        st.shutdown(Duration::from_secs(5));
        let again = open(&db, &dir.path().join("b")).unwrap();
        assert!(again.settings_json.is_some());
    }

    #[test]
    fn read_only_dir_fails_cleanly() {
        let r = open(
            Path::new("/proc/hwstats-test/x.db"),
            Path::new("/proc/hwstats-test/b"),
        );
        assert!(r.is_err());
    }
}
