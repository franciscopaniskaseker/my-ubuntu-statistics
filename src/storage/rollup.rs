//! Downsampling (raw -> 1 minute -> 1 hour), retention pruning and the
//! database size cap. Every statement is indexed by `(series_id, ts)`.

use super::schema::{meta_get, meta_set};
use crate::settings::History;
use anyhow::Result;
use rusqlite::{Connection, params};

fn series_ids(conn: &Connection) -> Result<Vec<i64>> {
    let mut st = conn.prepare_cached("SELECT id FROM series")?;
    let ids = st
        .query_map([], |r| r.get(0))?
        .collect::<Result<Vec<i64>, _>>()?;
    Ok(ids)
}

fn watermark(conn: &Connection, key: &str) -> Result<i64> {
    Ok(meta_get(conn, key)?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0))
}

/// Aggregate complete minutes of raw samples into `samples_1m`.
pub fn rollup_minutes(conn: &mut Connection, now: i64) -> Result<usize> {
    let until = ((now - 30) / 60) * 60;
    let from = watermark(conn, "rollup_1m_until")?;
    if until <= from {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    let mut n = 0;
    {
        let ids = series_ids(&tx)?;
        let mut st = tx.prepare_cached(
            "INSERT INTO samples_1m(series_id, ts, avg, min, max)
             SELECT series_id, (ts/60)*60 AS b, avg(value), min(value), max(value)
             FROM samples WHERE series_id=?1 AND ts>=?2 AND ts<?3 GROUP BY b
             ON CONFLICT(series_id, ts) DO UPDATE SET avg=excluded.avg, min=excluded.min, max=excluded.max",
        )?;
        for id in ids {
            n += st.execute(params![id, from, until])?;
        }
    }
    meta_set(&tx, "rollup_1m_until", &until.to_string())?;
    tx.commit()?;
    Ok(n)
}

/// Aggregate complete hours of 1-minute data into `samples_1h`.
pub fn rollup_hours(conn: &mut Connection, now: i64) -> Result<usize> {
    let until = ((now - 120) / 3600) * 3600;
    let from = watermark(conn, "rollup_1h_until")?;
    if until <= from {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    let mut n = 0;
    {
        let ids = series_ids(&tx)?;
        let mut st = tx.prepare_cached(
            "INSERT INTO samples_1h(series_id, ts, avg, min, max)
             SELECT series_id, (ts/3600)*3600 AS b, avg(avg), min(min), max(max)
             FROM samples_1m WHERE series_id=?1 AND ts>=?2 AND ts<?3 GROUP BY b
             ON CONFLICT(series_id, ts) DO UPDATE SET avg=excluded.avg, min=excluded.min, max=excluded.max",
        )?;
        for id in ids {
            n += st.execute(params![id, from, until])?;
        }
    }
    meta_set(&tx, "rollup_1h_until", &until.to_string())?;
    tx.commit()?;
    Ok(n)
}

fn delete_before(conn: &mut Connection, table: &str, cutoff: i64) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut n = 0;
    {
        let ids = series_ids(&tx)?;
        let mut st =
            tx.prepare_cached(&format!("DELETE FROM {table} WHERE series_id=?1 AND ts<?2"))?;
        for id in ids {
            n += st.execute(params![id, cutoff])?;
        }
    }
    tx.commit()?;
    Ok(n)
}

/// Apply retention to all tiers and drop series that have no data left.
pub fn prune(conn: &mut Connection, h: &History, now: i64) -> Result<usize> {
    let mut n = delete_before(conn, "samples", now - h.raw_retention_hours as i64 * 3600)?;
    n += delete_before(
        conn,
        "samples_1m",
        now - h.minute_retention_days as i64 * 86_400,
    )?;
    let hour_cut = now - h.hour_retention_days as i64 * 86_400;
    n += delete_before(conn, "samples_1h", hour_cut)?;
    conn.execute("DELETE FROM series WHERE last_seen < ?1", [hour_cut])?;
    Ok(n)
}

/// Bytes used by live pages.
pub fn used_bytes(conn: &Connection) -> Result<u64> {
    let pc: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let fl: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    let ps: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    Ok(((pc - fl).max(0) * ps) as u64)
}

/// Delete the oldest data until the database fits in `max_bytes`.
/// Raw data goes first, then minute data, then hour data.
pub fn enforce_size(conn: &mut Connection, max_bytes: u64, now: i64) -> Result<bool> {
    let mut trimmed = false;
    for table in ["samples", "samples_1m", "samples_1h"] {
        for _ in 0..6 {
            if used_bytes(conn)? <= max_bytes {
                return Ok(trimmed);
            }
            let oldest: Option<i64> =
                conn.query_row(&format!("SELECT min(ts) FROM {table}"), [], |r| r.get(0))?;
            let Some(oldest) = oldest else {
                break;
            };
            if oldest >= now {
                break;
            }
            let cutoff = oldest + ((now - oldest) / 4).max(60);
            delete_before(conn, table, cutoff)?;
            trimmed = true;
        }
    }
    Ok(trimmed)
}

pub fn vacuum_and_checkpoint(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA incremental_vacuum;")?;
    let _: (i64, i64, i64) = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema;

    fn db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Connection::open(dir.path().join("t.db")).unwrap();
        schema::configure(&c).unwrap();
        schema::migrate(&mut c).unwrap();
        c.execute(
            "INSERT INTO series(id, key, collector, metric, grp, group_label, label, unit, first_seen, last_seen)
             VALUES (1, 'a', 'cpu', 'm', '', '', 'A', '%', 0, 9999999999)",
            [],
        )
        .unwrap();
        (dir, c)
    }

    #[test]
    fn rollups_and_prune() {
        let (_d, mut c) = db();
        let base = 1_700_000_000 / 3600 * 3600;
        for i in 0..(2 * 3600 / 10) {
            c.execute(
                "INSERT INTO samples(series_id, ts, value) VALUES (1, ?1, ?2)",
                params![base + i * 10, (i % 6) as f64],
            )
            .unwrap();
        }
        let now = base + 2 * 3600 + 200;
        let n = rollup_minutes(&mut c, now).unwrap();
        assert_eq!(n, 120);
        let (avg, mn, mx): (f64, f64, f64) = c
            .query_row(
                "SELECT avg, min, max FROM samples_1m WHERE ts=?1",
                [base],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((avg, mn, mx), (2.5, 0.0, 5.0));
        // second run is a no-op
        assert_eq!(rollup_minutes(&mut c, now).unwrap(), 0);
        assert_eq!(rollup_hours(&mut c, now).unwrap(), 2);

        let h = History {
            raw_retention_hours: 1,
            ..History::default()
        };
        let removed = prune(&mut c, &h, now).unwrap();
        assert!(removed > 0);
        let left: i64 = c
            .query_row("SELECT count(*) FROM samples", [], |r| r.get(0))
            .unwrap();
        assert!(left < 720);
        vacuum_and_checkpoint(&c).unwrap();
    }

    #[test]
    fn size_cap_trims_oldest() {
        let (_d, mut c) = db();
        let tx = c.transaction().unwrap();
        for i in 0..200_000i64 {
            tx.execute(
                "INSERT INTO samples(series_id, ts, value) VALUES (1, ?1, 1.0)",
                [i],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let before = used_bytes(&c).unwrap();
        assert!(enforce_size(&mut c, before / 2, 200_000).unwrap());
        assert!(used_bytes(&c).unwrap() <= before / 2 + 65536);
        let min: i64 = c
            .query_row("SELECT min(ts) FROM samples", [], |r| r.get(0))
            .unwrap();
        assert!(min > 0);
    }
}
