//! Integrity check at startup and automatic recovery from backups.

use crate::backup;
use crate::util::time::{now_s, stamp};
use anyhow::{Result, anyhow};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Fresh,
    Healthy,
    Restored(PathBuf),
    RecreatedAfterCorruption,
}

/// Run `PRAGMA quick_check` on a database file.
pub fn quick_check(path: &Path) -> Result<()> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    let res: String = conn
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(|e| anyhow!("quick_check failed: {e}"))?;
    if res == "ok" {
        Ok(())
    } else {
        Err(anyhow!("quick_check: {res}"))
    }
}

/// Paths of the database and its WAL/SHM side files.
pub fn side_files(db: &Path) -> [PathBuf; 3] {
    let s = db.as_os_str().to_owned();
    let mut wal = s.clone();
    wal.push("-wal");
    let mut shm = s;
    shm.push("-shm");
    [db.to_path_buf(), PathBuf::from(wal), PathBuf::from(shm)]
}

/// Move a database (and side files) aside with the given suffix.
pub fn move_aside(db: &Path, suffix: &str) -> Result<PathBuf> {
    let [main, wal, shm] = side_files(db);
    let target = PathBuf::from(format!("{}.{suffix}", main.display()));
    std::fs::rename(&main, &target)?;
    for f in [wal, shm] {
        if f.exists() {
            let _ = std::fs::rename(&f, format!("{}.{suffix}", f.display()));
        }
    }
    Ok(target)
}

/// Check the database; quarantine it and restore the newest valid backup
/// when it is corrupt.
pub fn check_and_recover(db: &Path, backup_dir: &Path) -> Result<Outcome> {
    if !db.exists() {
        return Ok(Outcome::Fresh);
    }
    match quick_check(db) {
        Ok(()) => return Ok(Outcome::Healthy),
        Err(e) => tracing::error!("database {} failed integrity check: {e:#}", db.display()),
    }
    let moved = move_aside(db, &format!("corrupt-{}", stamp(now_s())))?;
    tracing::error!("corrupt database moved to {}", moved.display());
    for b in backup::list(backup_dir) {
        match backup::restore_file(&b.path, db) {
            Ok(()) => {
                tracing::warn!("restored database from backup {}", b.path.display());
                return Ok(Outcome::Restored(b.path));
            }
            Err(e) => tracing::warn!("backup {} is not usable: {e:#}", b.path.display()),
        }
    }
    tracing::warn!("no usable backup found; starting with an empty database");
    Ok(Outcome::RecreatedAfterCorruption)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_db_is_restored_from_backup() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("h.db");
        let bdir = dir.path().join("backups");
        {
            let mut c = Connection::open(&db).unwrap();
            crate::storage::schema::configure(&c).unwrap();
            crate::storage::schema::migrate(&mut c).unwrap();
            crate::storage::schema::meta_set(&c, "marker", "hello").unwrap();
        }
        let info = backup::create(&db, &bdir).unwrap();
        assert!(info.size > 0);
        // Corrupt the database file.
        let [_, wal, shm] = side_files(&db);
        let _ = std::fs::remove_file(wal);
        let _ = std::fs::remove_file(shm);
        std::fs::write(
            &db,
            b"this is definitely not a sqlite database, just garbage bytes......",
        )
        .unwrap();
        let out = check_and_recover(&db, &bdir).unwrap();
        assert!(matches!(out, Outcome::Restored(_)));
        let c = Connection::open(&db).unwrap();
        assert_eq!(
            crate::storage::schema::meta_get(&c, "marker")
                .unwrap()
                .as_deref(),
            Some("hello")
        );
        let corrupt = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains("corrupt-"));
        assert!(corrupt);
    }

    #[test]
    fn corrupt_db_without_backup_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("h.db");
        std::fs::write(&db, vec![7u8; 8192]).unwrap();
        let out = check_and_recover(&db, &dir.path().join("none")).unwrap();
        assert_eq!(out, Outcome::RecreatedAfterCorruption);
        assert!(!db.exists());
    }
}
