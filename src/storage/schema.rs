//! Schema and connection setup. The schema version lives in
//! `PRAGMA user_version`; migrations run inside a transaction.

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use std::time::Duration;

pub const VERSION: i64 = 1;

const V1: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS series (
    id          INTEGER PRIMARY KEY,
    key         TEXT NOT NULL UNIQUE,
    collector   TEXT NOT NULL,
    metric      TEXT NOT NULL,
    grp         TEXT NOT NULL,
    group_label TEXT NOT NULL,
    label       TEXT NOT NULL,
    unit        TEXT NOT NULL,
    first_seen  INTEGER NOT NULL,
    last_seen   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS samples (
    series_id INTEGER NOT NULL,
    ts        INTEGER NOT NULL,
    value     REAL NOT NULL,
    PRIMARY KEY (series_id, ts)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS samples_1m (
    series_id INTEGER NOT NULL,
    ts        INTEGER NOT NULL,
    avg       REAL NOT NULL,
    min       REAL NOT NULL,
    max       REAL NOT NULL,
    PRIMARY KEY (series_id, ts)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS samples_1h (
    series_id INTEGER NOT NULL,
    ts        INTEGER NOT NULL,
    avg       REAL NOT NULL,
    min       REAL NOT NULL,
    max       REAL NOT NULL,
    PRIMARY KEY (series_id, ts)
) WITHOUT ROWID;
";

/// Pragmas applied to every read/write connection.
pub fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!("database journal mode is {mode}, expected wal");
    }
    conn.execute_batch(
        "PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY;
         PRAGMA foreign_keys=OFF;",
    )?;
    Ok(())
}

pub fn version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

fn table_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table'",
        [],
        |r| r.get(0),
    )?)
}

/// Bring the schema to [`VERSION`]. Returns the version found on disk.
pub fn migrate(conn: &mut Connection) -> Result<i64> {
    let found = version(conn)?;
    if found > VERSION {
        bail!(
            "database schema version {found} is newer than this hwstats ({VERSION}); refusing to write to it"
        );
    }
    if found == VERSION {
        return Ok(found);
    }
    if found == 0 && table_count(conn)? == 0 {
        // Only effective before the first table exists.
        conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
    }
    let tx = conn.transaction()?;
    if found < 1 {
        tx.execute_batch(V1).context("creating schema v1")?;
    }
    tx.pragma_update(None, "user_version", VERSION)?;
    tx.commit()?;
    Ok(found)
}

pub fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut st = conn.prepare_cached("SELECT value FROM meta WHERE key=?1")?;
    let mut rows = st.query([key])?;
    Ok(match rows.next()? {
        Some(r) => Some(r.get(0)?),
        None => None,
    })
}

pub fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.prepare_cached("INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value")?
        .execute([key, value])?;
    Ok(())
}
