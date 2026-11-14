//! Daily compressed backups of the database with rotation.
//!
//! A backup is a `VACUUM INTO` snapshot (consistent, compact, taken without
//! stopping writes), verified with `quick_check`, gzip-compressed and moved
//! into place atomically. Rotation keeps the newest backup of each of the
//! last N days and of each of the last M weeks, within a total size cap.

use crate::settings::Settings;
use crate::storage::recovery::{move_aside, quick_check, side_files};
use crate::util::time::{day_index, now_s, parse_stamp, stamp, week_index};
use anyhow::{Context, Result, anyhow, bail};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify, watch};

const PREFIX: &str = "hwstats-";
const SUFFIX: &str = ".db.gz";
pub const MAX_AGE_S: i64 = 24 * 3600;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BackupInfo {
    pub file: String,
    #[serde(skip)]
    pub path: PathBuf,
    pub size: u64,
    pub created: i64,
}

/// Backups in `dir`, newest first.
pub fn list(dir: &Path) -> Vec<BackupInfo> {
    let mut v: Vec<BackupInfo> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().into_string().ok()?;
                    let st = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
                    let created = parse_stamp(st)?;
                    let size = e.metadata().ok()?.len();
                    Some(BackupInfo {
                        file: name,
                        path: e.path(),
                        size,
                        created,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| b.created.cmp(&a.created).then(b.file.cmp(&a.file)));
    v
}

fn free_bytes(dir: &Path) -> Option<u64> {
    crate::collectors::filesystem::statvfs(dir.to_str()?)
        .ok()
        .map(|u| u.avail)
}

/// Make files created by root (CLI use) owned like the reference file.
fn match_owner(paths: &[&Path], like: &Path) {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let Ok(m) = std::fs::metadata(like) else {
        return;
    };
    for p in paths {
        if p.exists() {
            let _ = std::os::unix::fs::chown(p, Some(m.uid()), Some(m.gid()));
        }
    }
}

fn fsync_dir(dir: &Path) {
    if let Ok(f) = File::open(dir) {
        let _ = f.sync_all();
    }
}

fn remove_stale_tmp(dir: &Path) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.filter_map(|e| e.ok()) {
            let n = e.file_name().to_string_lossy().to_string();
            if n.ends_with(".tmp") && (n.starts_with(PREFIX) || n.starts_with(".hwstats-")) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Create a verified, compressed backup of `db` in `dir`.
pub fn create(db: &Path, dir: &Path) -> Result<BackupInfo> {
    if !db.exists() {
        bail!("database {} does not exist", db.display());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    match_owner(&[dir], db.parent().unwrap_or(Path::new("/")));
    remove_stale_tmp(dir);

    let db_size: u64 = side_files(db)
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    if let Some(free) = free_bytes(dir)
        && free < db_size.saturating_mul(2)
    {
        bail!(
            "not enough free space in {} ({} MB free, {} MB needed)",
            dir.display(),
            free / 1_048_576,
            db_size.saturating_mul(2) / 1_048_576
        );
    }

    let mut ts = now_s();
    let mut name = format!("{PREFIX}{}{SUFFIX}", stamp(ts));
    while dir.join(&name).exists() {
        ts += 1;
        name = format!("{PREFIX}{}{SUFFIX}", stamp(ts));
    }
    let snap = dir.join(format!(".hwstats-snapshot-{}.tmp", stamp(ts)));
    let gz_tmp = dir.join(format!("{name}.tmp"));
    let result = (|| -> Result<BackupInfo> {
        {
            let conn = Connection::open_with_flags(
                db,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            conn.busy_timeout(Duration::from_secs(30))?;
            conn.execute("VACUUM INTO ?1", [snap.to_string_lossy().as_ref()])
                .context("VACUUM INTO")?;
        }
        quick_check(&snap).context("snapshot failed verification")?;
        {
            let input = File::open(&snap)?;
            let out = File::create(&gz_tmp)?;
            let mut enc = GzEncoder::new(BufWriter::new(out), Compression::default());
            std::io::copy(&mut BufReader::new(input), &mut enc)?;
            let w = enc.finish()?;
            let f = w.into_inner().map_err(|e| anyhow!("flush: {e}"))?;
            f.sync_all()?;
        }
        let final_path = dir.join(&name);
        std::fs::rename(&gz_tmp, &final_path)?;
        fsync_dir(dir);
        match_owner(&[&final_path], db);
        let size = std::fs::metadata(&final_path)?.len();
        Ok(BackupInfo {
            file: name.clone(),
            path: final_path,
            size,
            created: ts,
        })
    })();
    let _ = std::fs::remove_file(&snap);
    let _ = std::fs::remove_file(&gz_tmp);
    // A read-only open by root may have created side files owned by root.
    let [_, wal, shm] = side_files(db);
    match_owner(&[&wal, &shm], db);
    result
}

/// Delete backups outside the retention policy. Returns deleted files.
pub fn rotate(dir: &Path, keep_daily: u32, keep_weekly: u32, max_total: u64) -> Vec<PathBuf> {
    let all = list(dir);
    if all.is_empty() {
        return Vec::new();
    }
    let mut keep: HashSet<PathBuf> = HashSet::new();
    keep.insert(all[0].path.clone());
    let mut days = Vec::new();
    let mut weeks = Vec::new();
    for b in &all {
        let d = day_index(b.created);
        if !days.contains(&d) && days.len() < keep_daily as usize {
            days.push(d);
            keep.insert(b.path.clone());
        }
        let w = week_index(b.created);
        if !weeks.contains(&w) && weeks.len() < keep_weekly as usize {
            weeks.push(w);
            keep.insert(b.path.clone());
        }
    }
    let mut deleted = Vec::new();
    for b in &all {
        if !keep.contains(&b.path) && std::fs::remove_file(&b.path).is_ok() {
            deleted.push(b.path.clone());
        }
    }
    // Size cap: drop the oldest kept backups, never the newest one.
    let mut kept: Vec<&BackupInfo> = all.iter().filter(|b| keep.contains(&b.path)).collect();
    let mut total: u64 = kept.iter().map(|b| b.size).sum();
    while total > max_total && kept.len() > 1 {
        let b = kept.pop().expect("len > 1");
        if std::fs::remove_file(&b.path).is_ok() {
            deleted.push(b.path.clone());
        }
        total = total.saturating_sub(b.size);
    }
    deleted
}

/// Decompress a backup and verify it.
pub fn decompress_verified(gz: &Path, out: &Path) -> Result<()> {
    let input = File::open(gz).with_context(|| format!("opening {}", gz.display()))?;
    let mut dec = GzDecoder::new(BufReader::new(input));
    {
        let f = File::create(out)?;
        let mut w = BufWriter::new(f);
        std::io::copy(&mut dec, &mut w).context("decompressing")?;
        w.flush()?;
        w.into_inner()
            .map_err(|e| anyhow!("flush: {e}"))?
            .sync_all()?;
    }
    if let Err(e) = quick_check(out) {
        let _ = std::fs::remove_file(out);
        return Err(e.context("backup content failed verification"));
    }
    // quick_check may leave side files; remove them, the file is consistent.
    let [_, wal, shm] = side_files(out);
    let _ = std::fs::remove_file(wal);
    let _ = std::fs::remove_file(shm);
    Ok(())
}

/// Replace `db` with the content of a backup. The current database (if
/// any) is moved aside as `*.pre-restore-<stamp>`.
pub fn restore_file(gz: &Path, db: &Path) -> Result<()> {
    let tmp = PathBuf::from(format!("{}.restore.tmp", db.display()));
    let _ = std::fs::remove_file(&tmp);
    decompress_verified(gz, &tmp)?;
    if db.exists() {
        let moved = move_aside(db, &format!("pre-restore-{}", stamp(now_s())))?;
        tracing::info!("previous database kept as {}", moved.display());
    } else {
        let [_, wal, shm] = side_files(db);
        let _ = std::fs::remove_file(wal);
        let _ = std::fs::remove_file(shm);
    }
    std::fs::rename(&tmp, db)?;
    if let Some(parent) = db.parent() {
        fsync_dir(parent);
        match_owner(&[db], parent);
    }
    Ok(())
}

/// Lower the priority of the calling thread (CPU nice 19, idle I/O class).
fn lower_thread_priority() {
    // SAFETY: plain syscalls on the current thread id.
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 19);
        // IOPRIO_WHO_PROCESS = 1, IOPRIO_CLASS_IDLE = 3 << 13
        libc::syscall(libc::SYS_ioprio_set, 1, tid as libc::c_long, 3i64 << 13);
    }
}

/// Coordinates scheduled and on-demand backups.
pub struct Manager {
    pub db: PathBuf,
    pub dir: PathBuf,
    lock: Mutex<()>,
    pub trigger: Notify,
    pub last_error: std::sync::Mutex<Option<String>>,
}

impl Manager {
    pub fn new(db: PathBuf, dir: PathBuf) -> Self {
        Self {
            db,
            dir,
            lock: Mutex::new(()),
            trigger: Notify::new(),
            last_error: std::sync::Mutex::new(None),
        }
    }

    pub fn list(&self) -> Vec<BackupInfo> {
        list(&self.dir)
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|g| g.clone())
    }

    /// Run one backup + rotation on a low-priority thread.
    pub async fn run_now(&self, settings: &Settings) -> Result<BackupInfo> {
        let _g = self.lock.lock().await;
        let (db, dir) = (self.db.clone(), self.dir.clone());
        let b = settings.backups.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("hwstats-backup".into())
            .spawn(move || {
                lower_thread_priority();
                let r = std::panic::catch_unwind(|| {
                    let info = create(&db, &dir)?;
                    let deleted = rotate(
                        &dir,
                        b.keep_daily,
                        b.keep_weekly,
                        b.max_total_mb * 1024 * 1024,
                    );
                    for d in deleted {
                        tracing::info!("rotated out backup {}", d.display());
                    }
                    Ok::<_, anyhow::Error>(info)
                })
                .unwrap_or_else(|_| Err(anyhow!("backup thread panicked")));
                let _ = tx.send(r);
            })?;
        let r = rx
            .await
            .unwrap_or_else(|_| Err(anyhow!("backup thread vanished")));
        let mut le = self.last_error.lock().unwrap_or_else(|e| e.into_inner());
        match &r {
            Ok(i) => {
                tracing::info!("backup {} written ({} KB)", i.file, i.size / 1024);
                *le = None;
            }
            Err(e) => {
                crate::warn_limited!("backup:fail", "backup failed: {e:#}");
                *le = Some(format!("{e:#}"));
            }
        }
        r
    }

    /// Background loop: back up when the newest backup is older than 24 h.
    pub async fn run_scheduler(
        self: Arc<Self>,
        settings: watch::Receiver<Arc<Settings>>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut wait = Duration::from_secs(120);
        loop {
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.trigger.notified() => {}
                _ = shutdown.changed() => break,
            }
            wait = Duration::from_secs(3600);
            let s = settings.borrow().clone();
            if !s.backups.enabled || !self.db.exists() {
                continue;
            }
            let newest = list(&self.dir).first().map(|b| b.created);
            if newest.is_some_and(|t| now_s() - t < MAX_AGE_S) {
                continue;
            }
            if self.run_now(&s).await.is_err() {
                // Retry sooner after a failure.
                wait = Duration::from_secs(1800);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, ts: i64, size: usize) -> PathBuf {
        let p = dir.join(format!("{PREFIX}{}{SUFFIX}", stamp(ts)));
        std::fs::write(&p, vec![0u8; size]).unwrap();
        p
    }

    #[test]
    fn rotation_keeps_daily_and_weekly() {
        let dir = tempfile::tempdir().unwrap();
        let base = crate::util::time::days_from_civil(2026, 1, 5) * 86_400 + 3 * 3600; // Monday
        // one backup per day for 60 days, plus a second one on the newest day
        for d in 0..60 {
            touch(dir.path(), base + d * 86_400, 10);
        }
        touch(dir.path(), base + 59 * 86_400 + 600, 10);
        let deleted = rotate(dir.path(), 7, 4, u64::MAX);
        let left = list(dir.path());
        // 7 daily (the newest day's latest counts once) + weekly ones not already covered
        let days: HashSet<i64> = left.iter().map(|b| day_index(b.created)).collect();
        assert_eq!(days.len(), left.len());
        assert!(left.len() >= 7 && left.len() <= 11, "kept {}", left.len());
        assert_eq!(left[0].created, base + 59 * 86_400 + 600);
        assert_eq!(deleted.len() + left.len(), 61);
        let weeks: HashSet<i64> = left.iter().map(|b| week_index(b.created)).collect();
        assert!(weeks.len() >= 4);
    }

    #[test]
    fn rotation_respects_size_cap_but_keeps_newest() {
        let dir = tempfile::tempdir().unwrap();
        let base = 1_800_000_000;
        for d in 0..5 {
            touch(dir.path(), base + d * 86_400, 1000);
        }
        rotate(dir.path(), 7, 0, 2500);
        let left = list(dir.path());
        assert_eq!(left.len(), 2);
        assert_eq!(left[0].created, base + 4 * 86_400);
        rotate(dir.path(), 7, 0, 10);
        assert_eq!(list(dir.path()).len(), 1);
    }

    #[test]
    fn backup_restore_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("h.db");
        {
            let mut c = Connection::open(&db).unwrap();
            crate::storage::schema::configure(&c).unwrap();
            crate::storage::schema::migrate(&mut c).unwrap();
            crate::storage::schema::meta_set(&c, "k", "v1").unwrap();
        }
        let bdir = dir.path().join("backups");
        let info = create(&db, &bdir).unwrap();
        assert!(info.file.ends_with(".db.gz"));
        {
            let c = Connection::open(&db).unwrap();
            crate::storage::schema::meta_set(&c, "k", "v2").unwrap();
        }
        restore_file(&info.path, &db).unwrap();
        let c = Connection::open(&db).unwrap();
        assert_eq!(
            crate::storage::schema::meta_get(&c, "k")
                .unwrap()
                .as_deref(),
            Some("v1")
        );
        // a corrupt archive is rejected and the database is left alone
        let bad = bdir.join(format!("{PREFIX}{}{SUFFIX}", stamp(1)));
        std::fs::write(&bad, b"not gzip").unwrap();
        assert!(restore_file(&bad, &db).is_err());
        let c = Connection::open(&db).unwrap();
        assert_eq!(
            crate::storage::schema::meta_get(&c, "k")
                .unwrap()
                .as_deref(),
            Some("v1")
        );
    }
}
